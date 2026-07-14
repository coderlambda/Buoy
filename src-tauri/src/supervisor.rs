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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
}

impl Default for SupervisorOpts {
    fn default() -> Self {
        // Same numbers as the JS supervisor: 1s base, 30s cap, 8 attempts before Dead.
        SupervisorOpts { backoff_base_ms: 1000, backoff_max_ms: 30000, lifetime_attempt_cap: 8 }
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
    ) -> Arc<Self> {
        Arc::new(Supervisor {
            cfg, opts, factory, app_sink, state_sink, sleep,
            shared: Arc::new(Shared {
                backend: Mutex::new(None),
                state: Mutex::new(State::Connecting),
                attempts: AtomicU32::new(0),
                intentional: AtomicBool::new(false),
                generation: AtomicU32::new(0),
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
        // Tear down any previous backend first (so an old ssh can't keep streaming — the
        // doubled-output guard from the JS version).
        if let Some(old) = self.shared.backend.lock().unwrap().take() {
            old.kill();
        }
        let gen = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.set_state(State::Connecting);

        // Wrap the app sink: intercept Ready (=> Connected, reset attempts) and Exit (=> reconnect
        // or dead), forward everything else to the app. Tag by generation so a late exit from an
        // already-replaced backend is ignored.
        let me = Arc::clone(self);
        let app_sink = self.app_sink.clone();
        let wrapped: BackendSink = Arc::new(move |ev: BackendEvent| {
            match ev {
                BackendEvent::Ready => {
                    me.shared.attempts.store(0, Ordering::Relaxed);   // stable connection resets budget
                    me.set_state(State::Connected);
                    app_sink(BackendEvent::Ready);
                }
                BackendEvent::Exit => {
                    // Only the current generation's exit drives reconnect.
                    if gen == me.shared.generation.load(Ordering::Relaxed) {
                        me.on_exit();
                    }
                }
                other => app_sink(other),
            }
        });

        let cols = self.shared.cols.load(Ordering::Relaxed) as u16;
        let rows = self.shared.rows.load(Ordering::Relaxed) as u16;
        match (self.factory)(self.cfg.clone(), wrapped, cols, rows) {
            Ok(b) => { *self.shared.backend.lock().unwrap() = Some(b); }
            Err(e) => { crate::dlog!("supervisor: spawn failed: {}", e); self.on_exit(); }
        }
    }

    fn on_exit(self: &Arc<Self>) {
        if self.shared.intentional.load(Ordering::Relaxed) {
            self.set_state(State::Closed);
            return;
        }
        let attempts = self.shared.attempts.fetch_add(1, Ordering::Relaxed) + 1;
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
        thread::spawn(move || {
            sleep(Duration::from_millis(delay));
            if me.shared.intentional.load(Ordering::Relaxed) { return; }
            me.spawn();
        });
    }

    /// User-initiated retry from Dead.
    pub fn retry(self: &Arc<Self>) {
        if self.state() != State::Dead { return; }
        self.shared.attempts.store(0, Ordering::Relaxed);
        self.spawn();
    }

    /// Intentional close: stop respawning and tear the backend down.
    pub fn close(&self) {
        self.shared.intentional.store(true, Ordering::Relaxed);
        if let Some(b) = self.shared.backend.lock().unwrap().take() { b.kill(); }
        self.set_state(State::Closed);
    }

    // --- pass-throughs to the current backend ---
    pub fn write(&self, data: &str) { self.with_backend(|b| b.write(data)); }
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

    fn cfg() -> BackendConfig {
        BackendConfig { host: "h".into(), session: "s".into(), tmux_path: "t".into(),
            tmux_version: Some((3, 7)), base_args: vec![] }
    }

    // Immediate sleep so backoff doesn't slow tests (policy, not timing, is under test).
    fn nosleep() -> Arc<dyn Fn(Duration) + Send + Sync> { Arc::new(|_| {}) }

    fn opts_fast() -> SupervisorOpts {
        SupervisorOpts { backoff_base_ms: 0, backoff_max_ms: 0, lifetime_attempt_cap: 3 }
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

        let sup = Supervisor::new(cfg(), opts_fast(), factory, app_sink, state_sink, nosleep());
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
        let sup = Supervisor::new(cfg(), opts_fast(), factory, app_sink, state_sink, nosleep());
        sup.start(80, 24);

        let sink = || last_sink.lock().unwrap().clone().unwrap();
        let wait_spawns = |n: usize| { for _ in 0..1000 { if spawns.load(Ordering::Relaxed) >= n { return; } thread::sleep(Duration::from_millis(1)); } };
        // A successful reconnect (Ready) between exits must reset the budget so we never hit Dead.
        for i in 0..10 {
            sink()(BackendEvent::Exit);   // attempt++ then respawn (background)
            wait_spawns(2 + i);           // wait for the respawn so we Ready the NEW backend's sink
            sink()(BackendEvent::Ready);  // resets attempts to 0, Connected
        }
        assert_eq!(sup.state(), State::Connected, "stays healthy when each reconnect succeeds");
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
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep());
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
            Arc::new(|_| {}), Arc::new(|_| {}), nosleep());
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
}
