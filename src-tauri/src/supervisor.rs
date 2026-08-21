//! Reconnect supervisor (port of src/main/supervisor.js) for the Tauri backend. Owns ONE control
//! backend at a time and respawns ssh on a non-intentional exit (network drop) with capped
//! exponential backoff, reattaching the SAME tmux session — so a dropped connection recovers
//! without restarting the app. tmux keeps the session alive server-side; the client just reattaches.
//!
//! State machine (mirrors the JS supervisor, §5.1/§5.3):
//!   Connecting -> Connected (on first Ready) ; on exit -> Reconnecting (backoff) -> Connecting ;
//!   attempts over the cap -> Dead ; intentional close() -> Closed (no respawn).
//!
//! Testability: the backend factory and the sleep fn are injected, so the policy is unit-tested
//! deterministically without real ssh or real time.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::control_backend::{BackendConfig, BackendEvent, BackendSink, ControlBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    Connected,
    Reconnecting,
    Dead,
    Closed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Connecting => "connecting",
            State::Connected => "connected",
            State::Reconnecting => "reconnecting",
            State::Dead => "dead",
            State::Closed => "closed",
        }
    }
}

pub struct SupervisorOpts {
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub lifetime_attempt_cap: u32,
    /// A connection must stay up at least this long before its `Ready` is treated as STABLE and
    /// the retry budget is reset. A connect that attaches then dies within this window (the classic
    /// expired-credential flap: ssh briefly accepts, tmux attaches, then the link drops again) is
    /// NOT counted as stable — so those flaps accumulate attempts and eventually hit the cap
    /// instead of resetting it to 0 on every transient "Connected" and reconnecting forever.
    pub stable_after_ms: u64,
}

impl Default for SupervisorOpts {
    fn default() -> Self {
        // 1s base backoff, 30s cap, 10 attempts before Dead, 10s to qualify as a stable connection.
        SupervisorOpts { backoff_base_ms: 1000, backoff_max_ms: 30000, lifetime_attempt_cap: 10,
            stable_after_ms: 10000 }
    }
}

/// Reports a state change to the app layer (wired to `session:state` in lib.rs).
pub type StateSink = Arc<dyn Fn(State) + Send + Sync>;

/// Factory for a fresh backend given a sink + size. Injected so tests can supply a fake.
pub type BackendFactory =
    Arc<dyn Fn(BackendConfig, BackendSink, u16, u16) -> Result<Box<dyn BackendHandle>, String> + Send + Sync>;

/// The subset of a backend the supervisor drives. ControlBackend implements this; a fake does too.
/// Only `Send` is required (the supervisor guards the backend behind a Mutex) — ControlBackend's
/// MasterPty is Send-but-not-Sync, so requiring Sync here would exclude it.
pub trait BackendHandle: Send {
    fn write(&self, data: &str);
    /// Write to the tmux window that originated the input. Non-windowed/fake backends can keep
    /// the session-wide behaviour; control mode overrides this to avoid tab-switch routing races.
    fn write_to(&self, data: &str, _target: Option<&str>) { self.write(data); }
    fn resize(&self, cols: u16, rows: u16);
    fn new_window(&self);
    fn select_window(&self, win: &str);
    fn kill_window(&self, win: &str);
    fn rename_window(&self, win: &str, title: &str);
    fn capture_window(&self, win: &str);
    fn kill(&self);
}

impl BackendHandle for ControlBackend {
    fn write(&self, data: &str) { ControlBackend::write(self, data) }
    fn write_to(&self, data: &str, target: Option<&str>) {
        ControlBackend::write_to(self, data, target)
    }
    fn resize(&self, cols: u16, rows: u16) { ControlBackend::resize(self, cols, rows) }
    fn new_window(&self) { ControlBackend::new_window(self) }
    fn select_window(&self, win: &str) { ControlBackend::select_window(self, win) }
    fn kill_window(&self, win: &str) { ControlBackend::kill_window(self, win) }
    fn rename_window(&self, win: &str, title: &str) { ControlBackend::rename_window(self, win, title) }
    fn capture_window(&self, win: &str) { ControlBackend::capture_window(self, win) }
    fn kill(&self) { ControlBackend::kill(self) }
}

/// The default real factory: spawns a ControlBackend.
pub fn real_backend_factory() -> BackendFactory {
    Arc::new(|cfg, sink, cols, rows| {
        ControlBackend::spawn(cfg, sink, cols, rows)
            .map(|b| Box::new(b) as Box<dyn BackendHandle>)
            .map_err(|e| e.to_string())
    })
}

struct Shared {
    backend: Mutex<Option<Box<dyn BackendHandle>>>,
    state: Mutex<State>,
    attempts: AtomicU32,
    intentional: AtomicBool,   // close() requested -> stop respawning
    generation: AtomicU32,     // bumped each spawn; stale exit callbacks are ignored
    connected_at_ms: AtomicU64,   // now_ms() when the CURRENT connection reached Ready; 0 = not yet
    cols: AtomicU32,
    rows: AtomicU32,
}

pub struct Supervisor {
    cfg: BackendConfig,
    opts: SupervisorOpts,
    factory: BackendFactory,
    app_sink: BackendSink,     // forwards data/window/ready to the app
    state_sink: StateSink,
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,   // injected monotonic clock (millis); fakeable in tests
    shared: Arc<Shared>,
}

impl Supervisor {
    pub fn new(
        cfg: BackendConfig,
        opts: SupervisorOpts,
        factory: BackendFactory,
        app_sink: BackendSink,
        state_sink: StateSink,
        sleep: Arc<dyn Fn(Duration) + Send + Sync>,
        now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Supervisor {
            cfg, opts, factory, app_sink, state_sink, sleep, now_ms,
            shared: Arc::new(Shared {
                backend: Mutex::new(None),
                state: Mutex::new(State::Connecting),
                attempts: AtomicU32::new(0),
                intentional: AtomicBool::new(false),
                generation: AtomicU32::new(0),
                connected_at_ms: AtomicU64::new(0),
                cols: AtomicU32::new(80),
                rows: AtomicU32::new(24),
            }),
        })
    }

    /// Start the first connection. cols/rows seed the initial size.
    pub fn start(self: &Arc<Self>, cols: u16, rows: u16) {
        self.shared.cols.store(cols as u32, Ordering::Relaxed);
        self.shared.rows.store(rows as u32, Ordering::Relaxed);
        self.shared.intentional.store(false, Ordering::Relaxed);
        self.shared.attempts.store(0, Ordering::Relaxed);
        self.spawn();
    }

    fn set_state(&self, s: State) {
        let mut g = self.shared.state.lock().unwrap();
        if *g != s {
            *g = s;
            drop(g);
            (self.state_sink)(s);
        }
    }

    pub fn state(&self) -> State { *self.shared.state.lock().unwrap() }

    fn spawn(self: &Arc<Self>) {
        // Bump the generation BEFORE tearing down the old backend. kill() makes the old ssh exit,
        // and that exit arrives asynchronously (tmux `%exit` + reader EOF) a beat later. If we bumped
        // AFTER kill(), that exit would still see its own generation as current, pass the gate, and
        // run on_exit() — scheduling a SECOND respawn that races this one. The two ssh control
        // clients then evict each other forever via `new-session -D` (the force-reconnect flap loop).
        // Bumping first means the dying backend's generation is already stale, so its exit is dropped.
        let gen = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // Tear down any previous backend (so an old ssh can't keep streaming — the doubled-output
        // guard from the JS version). Its late exit is now gen-stale and ignored.
        //
        // Take the backend out in its OWN scope so the mutex guard is released before kill() runs.
        // `if let Some(x) = lock().take() { .. }` would hold the guard for the whole body (edition
        // 2021 temporary lifetime), and kill() joins threads / runs sink callbacks — any of which
        // touching shared.backend (e.g. with_backend) would deadlock.
        let old = self.shared.backend.lock().unwrap().take();
        if let Some(old) = old {
            old.kill();
        }
        self.shared.connected_at_ms.store(0, Ordering::Relaxed);   // this attempt hasn't reached Ready yet
        self.set_state(State::Connecting);

        // Wrap the app sink: intercept Ready (=> Connected, stamp connect time) and Exit (=> reconnect
        // or dead), forward everything else to the app. Tag by generation so a late exit from an
        // already-replaced backend is ignored.
        //
        // NOTE: Ready does NOT reset the attempt budget here. A connection that attaches then dies
        // within `stable_after_ms` (an expired-credential flap: ssh briefly accepts, tmux attaches,
        // link drops) would otherwise reset the budget on every transient "Connected" and reconnect
        // forever. Instead we stamp when Ready arrived; on_exit resets the budget only if the
        // connection stayed up long enough to be considered stable.
        let me = Arc::clone(self);
        let app_sink = self.app_sink.clone();
        // Per-generation "already handled its exit" latch. A SINGLE backend death can surface twice:
        // tmux sends `%exit` (a control event) AND the reader thread then hits EOF — control_backend
        // emits BackendEvent::Exit for both. Without this latch on_exit() would run twice for one
        // death, double-incrementing the attempt budget and scheduling TWO respawns; the two ssh
        // control clients then evict each other forever via `new-session -D` (the force-reconnect
        // flap loop). The generation check drops a REPLACED backend's late exit; this latch collapses
        // the %exit+EOF pair from the CURRENT backend into exactly one reconnect.
        let exited = Arc::new(AtomicBool::new(false));
        let wrapped: BackendSink = Arc::new(move |ev: BackendEvent| {
            // EVERY event is generation-gated, not just Exit. A replaced backend can still emit after
            // it was killed — its uncancellable 5s ready-fallback timer fires Ready, and a topology
            // reply parsed just before the kill emits WindowAdd/WindowActive. Applying those to the
            // live session flipped Dead -> Connected (bricking Reconnect, which requires Dead) and
            // injected phantom tabs from the dead connection's topology.
            if gen != me.shared.generation.load(Ordering::Relaxed) {
                crate::dlog!("supervisor: dropping stale gen-{} event {:?}", gen, ev);
                return;
            }
            match ev {
                BackendEvent::Ready => {
                    me.shared.connected_at_ms.store((me.now_ms)().max(1), Ordering::Relaxed);
                    me.set_state(State::Connected);
                    app_sink(BackendEvent::Ready);
                }
                BackendEvent::Exit => {
                    // Only the FIRST exit drives reconnect (idempotent per death: one backend death
                    // surfaces as both tmux `%exit` and the reader-thread EOF).
                    if !exited.swap(true, Ordering::Relaxed) {
                        me.on_exit();
                    }
                }
                other => app_sink(other),
            }
        });

        let cols = self.shared.cols.load(Ordering::Relaxed) as u16;
        let rows = self.shared.rows.load(Ordering::Relaxed) as u16;
        match (self.factory)(self.cfg.clone(), wrapped, cols, rows) {
            Ok(b) => {
                // Install the new backend, and if a concurrent spawn() already put one there, KILL the
                // displaced one — a plain assignment would just drop it. There is no Drop impl on
                // ControlBackend, so a dropped backend leaks its ssh child: that orphan keeps its tmux
                // control client attached and, because it ran `new-session -D`, mutually evicts the
                // surviving client (the connect/break flap loop). Same scope discipline as above:
                // release the guard before kill().
                let displaced = {
                    let mut slot = self.shared.backend.lock().unwrap();
                    slot.replace(b)
                };
                if let Some(orphan) = displaced {
                    crate::dlog!("supervisor: killing backend displaced by a concurrent spawn");
                    orphan.kill();
                }
            }
            Err(e) => { crate::dlog!("supervisor: spawn failed: {}", e); self.on_exit(); }
        }
    }

    fn on_exit(self: &Arc<Self>) {
        if self.shared.intentional.load(Ordering::Relaxed) {
            self.set_state(State::Closed);
            return;
        }
        // If the connection we just lost was STABLE (reached Ready and stayed up >= stable_after_ms),
        // reset the retry budget — this was a healthy session that dropped, so give it a fresh set of
        // attempts. A flap that never reached that threshold does NOT reset, so repeated flaps
        // accumulate toward the cap and eventually go Dead instead of looping forever.
        let connected_at = self.shared.connected_at_ms.swap(0, Ordering::Relaxed);
        let was_stable = connected_at != 0
            && (self.now_ms)().saturating_sub(connected_at) >= self.opts.stable_after_ms;
        // Single atomic RMW: a stable drop resets the budget to 1 (this attempt), otherwise increment.
        // Doing this as store(0) followed by fetch_add left a window where a concurrent exit's
        // increment could be lost, letting the supervisor retry past its cap instead of going Dead.
        let prev = self
            .shared
            .attempts
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |a| {
                Some(if was_stable { 1 } else { a.saturating_add(1) })
            })
            .unwrap_or(0);   // closure always returns Some, so this never fires
        let attempts = if was_stable { 1 } else { prev.saturating_add(1) };
        if attempts > self.opts.lifetime_attempt_cap {
            crate::dlog!("supervisor: attempt cap reached -> dead");
            self.set_state(State::Dead);   // stop; no hot-loop / auth storm
            return;
        }
        self.set_state(State::Reconnecting);
        let delay = std::cmp::min(
            self.opts.backoff_base_ms.saturating_mul(1u64 << (attempts - 1).min(20)),
            self.opts.backoff_max_ms,
        );
        crate::dlog!("supervisor: reconnect attempt {} in {}ms", attempts, delay);
        let me = Arc::clone(self);
        let sleep = self.sleep.clone();
        // Tag this timer with the generation that was current when it was scheduled. The timer thread
        // is detached and cannot be interrupted, so on wake it must check whether it is still the
        // relevant one: any spawn() since (a user force_reconnect / retry that already reconnected
        // successfully) bumped the generation, and this timer firing anyway would kill that healthy
        // backend and reconnect on top of it — the user's manual reconnect breaking seconds later.
        let scheduled_gen = self.shared.generation.load(Ordering::Relaxed);
        thread::spawn(move || {
            sleep(Duration::from_millis(delay));
            if me.shared.intentional.load(Ordering::Relaxed) { return; }
            if scheduled_gen != me.shared.generation.load(Ordering::Relaxed) {
                crate::dlog!("supervisor: backoff timer for gen {} superseded, not respawning", scheduled_gen);
                return;
            }
            me.spawn();
        });
    }

    /// User-initiated retry from Dead.
    pub fn retry(self: &Arc<Self>) {
        if self.state() != State::Dead { return; }
        self.shared.attempts.store(0, Ordering::Relaxed);
        self.spawn();
    }

    /// User-initiated FORCE reconnect from ANY live state (connected / connecting / reconnecting /
    /// dead) — tears down the current backend and reattaches fresh with a reset budget. Unlike
    /// retry() this doesn't require Dead: it's the "my session is wedged, reconnect now" button.
    /// A no-op only after an intentional close() (the session is gone; don't resurrect it).
    pub fn force_reconnect(self: &Arc<Self>) {
        if self.shared.intentional.load(Ordering::Relaxed) { return; }
        self.shared.attempts.store(0, Ordering::Relaxed);
        self.spawn();   // spawn() kills any existing backend first and bumps the generation, so the
                        // old ssh's later Exit is ignored (no double-reconnect).
    }

    /// Intentional close: stop respawning and tear the backend down.
    pub fn close(&self) {
        self.shared.intentional.store(true, Ordering::Relaxed);
        // Release the guard before kill() (see spawn()): kill() runs foreign code that may re-enter.
        let b = self.shared.backend.lock().unwrap().take();
        if let Some(b) = b { b.kill(); }
        self.set_state(State::Closed);
    }

    // --- pass-throughs to the current backend ---
    pub fn write(&self, data: &str) { self.with_backend(|b| b.write(data)); }
    pub fn write_to(&self, data: &str, target: Option<&str>) {
        self.with_backend(|b| b.write_to(data, target));
    }
    pub fn resize(&self, cols: u16, rows: u16) {
        self.shared.cols.store(cols as u32, Ordering::Relaxed);
        self.shared.rows.store(rows as u32, Ordering::Relaxed);
        self.with_backend(|b| b.resize(cols, rows));
    }
    pub fn new_window(&self) { self.with_backend(|b| b.new_window()); }
    pub fn select_window(&self, win: &str) { self.with_backend(|b| b.select_window(win)); }
    pub fn kill_window(&self, win: &str) { self.with_backend(|b| b.kill_window(win)); }
    pub fn rename_window(&self, win: &str, title: &str) { self.with_backend(|b| b.rename_window(win, title)); }
    pub fn capture_window(&self, win: &str) { self.with_backend(|b| b.capture_window(win)); }

    fn with_backend(&self, f: impl FnOnce(&dyn BackendHandle)) {
        if let Some(b) = self.shared.backend.lock().unwrap().as_deref() { f(b); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // A fake backend that records spawns and lets the test drive Ready/Exit via the sink.
    struct FakeBackend { sink: BackendSink, killed: Arc<AtomicBool> }
    impl BackendHandle for FakeBackend {
        fn write(&self, _: &str) {}
        fn resize(&self, _: u16, _: u16) {}
        fn new_window(&self) {}
        fn select_window(&self, _: &str) {}
        fn kill_window(&self, _: &str) {}
        fn rename_window(&self, _: &str, _: &str) {}
        fn capture_window(&self, _: &str) {}
        fn kill(&self) { self.killed.store(true, Ordering::Relaxed); }
    }

    use crate::transport::Transport;

    fn cfg() -> BackendConfig {
        BackendConfig { host: "h".into(), session: "s".into(), tmux_path: "t".into(),
            tmux_version: Some((3, 7)), socket: "default".into(), recovery_windows: vec![],
            base_args: vec![], transport: Transport::Ssh }
    }

    // Immediate sleep so backoff doesn't slow tests (policy, not timing, is under test).
    fn nosleep() -> Arc<dyn Fn(Duration) + Send + Sync> { Arc::new(|_| {}) }

    // A clock the test drives by hand (millis). Default 0 so every Ready is treated as an instant
    // flap (never stable) unless the test advances it — keeps existing tests hitting the cap.
    fn fake_clock() -> (Arc<AtomicU64>, Arc<dyn Fn() -> u64 + Send + Sync>) {
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        (t, Arc::new(move || tc.load(Ordering::Relaxed)))
    }

    fn opts_fast() -> SupervisorOpts {
        SupervisorOpts { backoff_base_ms: 0, backoff_max_ms: 0, lifetime_attempt_cap: 3,
            stable_after_ms: 10000 }
    }

    #[test]
    fn tc_sup_reconnects_on_exit_until_cap_then_dead() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let states = Arc::new(Mutex::new(Vec::<State>::new()));

        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_cfg, sink, _c, _r| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let st = states.clone();
        let state_sink: StateSink = Arc::new(move |s| st.lock().unwrap().push(s));
        let app_sink: BackendSink = Arc::new(|_| {});

        let (_clock, now) = fake_clock();   // stays at 0: pure exits, no stable connection
        let sup = Supervisor::new(cfg(), opts_fast(), factory, app_sink, state_sink, nosleep(), now);
        sup.start(80, 24);
        assert_eq!(spawns.load(Ordering::Relaxed), 1, "one spawn on start");

        // Each Exit (no Ready) counts an attempt; cap is 3, so the 4th exit -> Dead. Respawn runs
        // on a background thread, so after firing an exit we WAIT for the respawn (spawn count to
        // increment) before firing the next — mirrors real timing and dodges the race.
        let wait_spawns = |n: usize| {
            for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= n { return true; } thread::sleep(Duration::from_millis(1)); }
            false
        };
        let fire_exit = || { let s = last_sink.lock().unwrap().clone().unwrap(); s(BackendEvent::Exit); };
        fire_exit(); assert!(wait_spawns(2), "attempt 1 respawns");
        fire_exit(); assert!(wait_spawns(3), "attempt 2 respawns");
        fire_exit(); assert!(wait_spawns(4), "attempt 3 respawns");
        fire_exit(); // attempt 4 > cap -> Dead, no respawn
        for _ in 0..1000 { if sup.state() == State::Dead { break; } thread::sleep(Duration::from_millis(1)); }
        assert_eq!(sup.state(), State::Dead);
        assert_eq!(spawns.load(Ordering::Relaxed), 4, "respawned up to the cap then stopped");
    }

    #[test]
    fn tc_sup_ready_resets_attempt_budget() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let app_sink: BackendSink = Arc::new(|_| {});
        let state_sink: StateSink = Arc::new(|_| {});
        let (clock, now) = fake_clock();
        let sup = Supervisor::new(cfg(), opts_fast(), factory, app_sink, state_sink, nosleep(), now);
        sup.start(80, 24);

        let sink = || last_sink.lock().unwrap().clone().unwrap();
        let wait_spawns = |n: usize| { for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= n { return; } thread::sleep(Duration::from_millis(1)); } };
        // A STABLE reconnect (Ready, then up past stable_after_ms) between exits must reset the
        // budget so we never hit Dead. Advance the clock by >= stable_after_ms while "connected".
        for i in 0..10 {
            sink()(BackendEvent::Ready);              // Connected; stamps connected_at at current time
            clock.fetch_add(20000, Ordering::Relaxed); // stay up 20s (>= 10s stable window)
            sink()(BackendEvent::Exit);               // stable drop -> resets budget, then attempt++
            wait_spawns(2 + i);                        // wait for the respawn so next Ready hits the new sink
            assert_ne!(sup.state(), State::Dead, "a stable connection must reset the budget, never Dead");
        }
    }

    #[test]
    fn tc_sup_intentional_close_suppresses_respawn() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        sup.close();
        assert_eq!(sup.state(), State::Closed);
        // An exit AFTER close (e.g. the killed ssh) must NOT respawn.
        last_sink.lock().unwrap().clone().unwrap()(BackendEvent::Exit);
        assert_eq!(spawns.load(Ordering::Relaxed), 1, "no respawn after intentional close");
        assert_eq!(sup.state(), State::Closed);
    }

    #[test]
    fn tc_sup_retry_from_dead() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        let sink = || last_sink.lock().unwrap().clone().unwrap();
        let wait_spawns = |n: usize| { for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= n { return; } thread::sleep(Duration::from_millis(1)); } };
        sink()(BackendEvent::Exit); wait_spawns(2);
        sink()(BackendEvent::Exit); wait_spawns(3);
        sink()(BackendEvent::Exit); wait_spawns(4);
        sink()(BackendEvent::Exit);   // > cap -> Dead
        for _ in 0..1000 { if sup.state() == State::Dead { break; } thread::sleep(Duration::from_millis(1)); }
        assert_eq!(sup.state(), State::Dead);
        let before = spawns.load(Ordering::Relaxed);
        sup.retry();
        assert_eq!(sup.state(), State::Connecting);
        assert_eq!(spawns.load(Ordering::Relaxed), before + 1, "retry spawns again");
    }

    #[test]
    fn tc_sup_force_reconnect_from_connected_respawns() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicBool::new(false));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone(); let kd = killed.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: kd.clone() }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        let sink = || last_sink.lock().unwrap().clone().unwrap();
        sink()(BackendEvent::Ready);
        assert_eq!(sup.state(), State::Connected);
        // Force reconnect from a HEALTHY connection (unlike retry, which requires Dead): the current
        // backend is torn down and a fresh one spawned.
        let before = spawns.load(Ordering::Relaxed);
        sup.force_reconnect();
        assert_eq!(spawns.load(Ordering::Relaxed), before + 1, "force_reconnect respawns while connected");
        assert!(killed.load(Ordering::Relaxed), "old backend was killed");
        assert_eq!(sup.state(), State::Connecting);
    }

    #[test]
    fn tc_sup_force_reconnect_noop_after_close() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        sup.close();
        let before = spawns.load(Ordering::Relaxed);
        sup.force_reconnect();   // intentionally closed -> must NOT resurrect the session
        assert_eq!(spawns.load(Ordering::Relaxed), before, "no respawn after intentional close");
        assert_eq!(sup.state(), State::Closed);
    }

    // The credential-expiry flap: each attempt REACHES Ready (ssh briefly accepts, tmux attaches)
    // but drops again almost immediately — under the stable window. These transient "Connected"
    // flashes must NOT reset the retry budget, so the supervisor still reaches Dead at the cap
    // instead of reconnecting forever. (This is the regression the fix targets.)
    #[test]
    fn tc_sup_transient_ready_flap_does_not_reset_budget_reaches_dead() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let (clock, now) = fake_clock();
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), now);
        sup.start(80, 24);
        let sink = || last_sink.lock().unwrap().clone().unwrap();
        let wait_spawns = |n: usize| { for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= n { return; } thread::sleep(Duration::from_millis(1)); } };
        // cap is 3. Each round: Ready, advance only 100ms (< 10s stable window), Exit.
        sink()(BackendEvent::Ready); clock.fetch_add(100, Ordering::Relaxed); sink()(BackendEvent::Exit); wait_spawns(2);
        sink()(BackendEvent::Ready); clock.fetch_add(100, Ordering::Relaxed); sink()(BackendEvent::Exit); wait_spawns(3);
        sink()(BackendEvent::Ready); clock.fetch_add(100, Ordering::Relaxed); sink()(BackendEvent::Exit); wait_spawns(4);
        sink()(BackendEvent::Ready); clock.fetch_add(100, Ordering::Relaxed); sink()(BackendEvent::Exit); // > cap
        for _ in 0..1000 { if sup.state() == State::Dead { break; } thread::sleep(Duration::from_millis(1)); }
        assert_eq!(sup.state(), State::Dead, "flapping connect/attach/drop still reaches Dead, not an endless loop");
    }

    // REGRESSION (force-reconnect flap): a SINGLE backend death surfaces as TWO Exit events — tmux
    // `%exit` AND the reader-thread EOF. Both hit the same generation's sink. They must collapse into
    // exactly ONE on_exit() (one attempt, one respawn); otherwise the budget double-increments and
    // two ssh control clients race, evicting each other via `new-session -D` forever.
    #[test]
    fn tc_sup_double_exit_same_backend_reconnects_once() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let last_sink: Arc<Mutex<Option<BackendSink>>> = Arc::new(Mutex::new(None));
        let sp = spawns.clone(); let ls = last_sink.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            *ls.lock().unwrap() = Some(sink.clone());
            Ok(Box::new(FakeBackend { sink, killed: Arc::new(AtomicBool::new(false)) }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        assert_eq!(spawns.load(Ordering::Relaxed), 1);
        // Grab THIS backend's sink and fire both of its exit signals (%exit then EOF) before the
        // respawn swaps last_sink.
        let s = last_sink.lock().unwrap().clone().unwrap();
        s(BackendEvent::Exit);
        s(BackendEvent::Exit);
        for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= 2 { break; } thread::sleep(Duration::from_millis(1)); }
        // Exactly one respawn (2 total spawns), never two from the single death.
        thread::sleep(Duration::from_millis(20));
        assert_eq!(spawns.load(Ordering::Relaxed), 2, "double exit from one backend -> exactly one respawn");
        assert_eq!(sup.shared.attempts.load(Ordering::Relaxed), 1, "attempt budget incremented once, not twice");
    }

    // REGRESSION (force-reconnect flap): the real timing race. A live backend's kill() makes its ssh
    // exit, and that exit surfaces SYNCHRONOUSLY-ish, WHILE spawn() is still tearing down the old
    // backend — i.e. before the fresh backend exists. If spawn() bumped the generation only AFTER
    // kill(), that exit would still see its own generation as current, pass the gate, and schedule a
    // SECOND respawn racing the fresh one (the loop). Bumping the generation BEFORE kill() makes the
    // dying backend's gen already stale, so its kill-triggered exit is dropped.
    //
    // We model the real timing with a backend whose kill() fires Exit on its own sink synchronously.
    #[test]
    fn tc_sup_kill_triggered_exit_does_not_double_respawn() {
        // A backend that emits Exit from kill() — exactly what a real ssh does when torn down.
        struct KillFiresExit { sink: BackendSink }
        impl BackendHandle for KillFiresExit {
            fn write(&self, _: &str) {}
            fn resize(&self, _: u16, _: u16) {}
            fn new_window(&self) {}
            fn select_window(&self, _: &str) {}
            fn kill_window(&self, _: &str) {}
            fn rename_window(&self, _: &str, _: &str) {}
            fn capture_window(&self, _: &str) {}
            fn kill(&self) { (self.sink)(BackendEvent::Exit); }   // ssh dies -> exit surfaces during kill
        }

        let spawns = Arc::new(AtomicUsize::new(0));
        let sp = spawns.clone();
        let factory: BackendFactory = Arc::new(move |_c, sink, _cc, _rr| {
            sp.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(KillFiresExit { sink }) as Box<dyn BackendHandle>)
        });
        let sup = Supervisor::new(cfg(), opts_fast(), factory,
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep(), fake_clock().1);
        sup.start(80, 24);
        assert_eq!(spawns.load(Ordering::Relaxed), 1);

        // force_reconnect -> spawn(): kills the old backend (which fires its Exit mid-teardown) and
        // spawns exactly one fresh backend. If the old exit weren't gen-stale it would schedule an
        // extra respawn here.
        sup.force_reconnect();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(spawns.load(Ordering::Relaxed), 2,
            "force_reconnect spawns exactly once; the old backend's kill-exit must be gen-stale, not a 2nd respawn");
        assert_eq!(sup.state(), State::Connecting, "settled on the fresh backend, not flapping");
    }
}
