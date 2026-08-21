//! Disk-backed session list (DESIGN.md §5.2). Port of src/main/sessionStore.js. The persisted
//! file is UNTRUSTED: re-validate host/session on load and drop invalid entries.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::validation::{parse_host, validate_session};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryTab {
    pub window: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub last_command: String,
}

/// A reconstructible description of one tmux window. This is deliberately not a process
/// checkpoint: recovery creates a shell in `cwd` and labels it with the last foreground command,
/// but never re-executes an arbitrary command after a host restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryWindow {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub active: bool,
}

// Serialized to/from the renderer AND disk in camelCase, so the JS side reads meta.tmuxPath /
// meta.tmuxVersion directly (a snake/camel mismatch here silently dropped the persisted tmux
// path -> re-probe on every reconnect -> wrong socket -> couldn't reattach existing sessions).
// `alias` keeps older snake_case store files loadable (migrated to camelCase on next save).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub host: String,
    pub session: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default, alias = "tmux_path")]
    pub tmux_path: Option<String>,
    #[serde(default, alias = "tmux_version")]
    pub tmux_version: Option<(u32, u32)>,
    /// None means a Buoy-owned versioned socket. Imported sessions use `default`, the user's
    /// ordinary tmux server, so Buoy and existing terminal clients see the same session.
    #[serde(default, alias = "socket_name")]
    pub socket_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub order: i64,
    // Did the LAST attach for this row get far enough to produce output? The cached tmuxPath /
    // tmuxVersion above also selects the version-tagged socket, so it can't be re-probed freely
    // (a changed version = a different socket = the live remote server is stranded). This flag is
    // how create_session tells "cache is good, reuse it" from "the cache never worked, re-probe":
    // cleared on every attach attempt, set once the backend produces Ready/output.
    #[serde(default)]
    pub attach_ok: bool,
    // §20: sidebar/tab customization, all persisted so they survive restart.
    #[serde(default)]
    pub color: Option<String>,                    // project accent color (hex like "#89b4fa")
    #[serde(default)]
    pub last_tab: Option<String>,                 // last-active tmux window id ("@N")
    #[serde(default)]
    pub tab_order: Vec<String>,                   // custom tab order (window ids); missing -> tmux order
    #[serde(default)]
    pub tab_colors: std::collections::BTreeMap<String, String>,   // window id -> accent color
    // Explicit Close snapshots and ends tmux, then archives the row for reconstruction. Legacy rows
    // default to active so upgrades do not unexpectedly hide existing sessions.
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub archived_at: Option<u64>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub recovery_tabs: Vec<RecoveryTab>,
    #[serde(default)]
    pub restore_pending: bool,
    /// Continuously observed active-pane state used only when a host reboot removes a live tmux
    /// server. This complements `recovery_tabs`, which belongs to explicit Close/History.
    #[serde(default)]
    pub recovery_windows: Vec<RecoveryWindow>,
}

fn default_transport() -> String { "ssh".into() }
fn default_mode() -> String { "plain".into() }

fn safe_tmux_path(p: &Option<String>) -> Option<String> {
    match p {
        Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')) => {
            Some(s.clone())
        }
        _ => None,
    }
}

fn safe_socket_name(p: &Option<String>) -> Option<String> {
    match p {
        Some(s) if !s.is_empty() && s.len() <= 128
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) => Some(s.clone()),
        _ => None,
    }
}

fn clean_text(s: &str, limit: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(limit).collect::<String>().trim().to_string()
}

fn clean_recovery_windows(windows: &[RecoveryWindow]) -> Vec<RecoveryWindow> {
    let mut out = Vec::new();
    let mut active_seen = false;
    for window in windows.iter().take(64) {
        // pane_current_path is absolute. Refusing other values both rejects corrupt state and keeps
        // the later tmux `-c` argument constrained even though sessions.json is untrusted input.
        if window.cwd.len() > 4096 || window.cwd.chars().any(|c| c.is_control()) { continue; }
        let cwd = window.cwd.clone();
        if !cwd.starts_with('/') { continue; }
        let active = window.active && !active_seen;
        active_seen |= active;
        out.push(RecoveryWindow {
            name: clean_text(&window.name, 100),
            cwd,
            command: clean_text(&window.command, 100),
            active,
        });
    }
    if !out.is_empty() && !active_seen { out[0].active = true; }
    out
}

pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> Self {
        SessionStore { path }
    }

    pub fn load(&self) -> Vec<SessionMeta> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(_) => return Vec::new(), // missing file => empty
        };
        let parsed: Vec<SessionMeta> = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(_) => return Vec::new(), // corrupt => empty, no throw
        };
        let mut out: Vec<SessionMeta> = Vec::new();
        for mut e in parsed {
            if validate_session(&e.session).is_err() { continue; }
            // A LOCAL session legitimately has no host (transport "local"), so the host check only
            // applies to rows that name a remote. Rejecting an empty host here would silently drop
            // every persisted local session on load.
            if e.transport == "local" {
                if !e.host.is_empty() { continue; }   // a local row with a host is malformed
            } else if parse_host(&e.host).is_err() {
                continue;
            }
            if !["ssh", "mosh", "et", "local"].contains(&e.transport.as_str()) {
                e.transport = "ssh".into();
            }
            if !["control", "plain", "local"].contains(&e.mode.as_str()) { e.mode = "plain".into(); }
            e.tmux_path = safe_tmux_path(&e.tmux_path);
            e.socket_name = safe_socket_name(&e.socket_name);
            e.recovery_windows = clean_recovery_windows(&e.recovery_windows);
            if e.title.is_none() { e.title = Some(e.session.clone()); }
            out.push(e);
        }
        out.sort_by_key(|e| e.order);
        out
    }

    /// Flip the `attach_ok` cache-confidence flag for one row, leaving everything else alone.
    /// Called with `true` when a backend reaches Ready (the persisted tmuxPath/tmuxVersion pair
    /// demonstrably works) and with `false` when an attach starts (so a crash mid-attach leaves the
    /// cache marked unproven and the next create_session re-probes).
    pub fn set_attach_ok(&self, id: &str, ok: bool) {
        let mut list = self.load();
        let mut hit = false;
        for e in list.iter_mut() {
            if e.id == id && e.attach_ok != ok { e.attach_ok = ok; hit = true; }
        }
        if hit { self.save(&list); }
    }

    pub fn update_session(&self, id: &str, update: impl FnOnce(&mut SessionMeta)) -> bool {
        let mut list = self.load();
        let Some(session) = list.iter_mut().find(|session| session.id == id) else { return false };
        update(session);
        self.save(&list);
        true
    }

    /// Move one persisted session into or out of History without changing its tmux identity,
    /// cached attach metadata, tab preferences, or ordering. Returns false for an unknown id.
    pub fn set_archived(&self, id: &str, archived: bool, archived_at: Option<u64>) -> bool {
        self.update_session(id, |session| {
            session.archived = archived;
            session.archived_at = if archived { archived_at } else { None };
        })
    }

    /// Persist only meaningful topology changes; the backend polls periodically to catch `cd`, so
    /// writing an identical JSON file every time would cause needless disk churn.
    pub fn set_recovery_windows(&self, id: &str, windows: &[RecoveryWindow]) {
        let clean = clean_recovery_windows(windows);
        if clean.is_empty() { return; }
        let mut list = self.load();
        let mut hit = false;
        for e in list.iter_mut() {
            if e.id == id && e.recovery_windows != clean {
                e.recovery_windows = clean.clone();
                hit = true;
            }
        }
        if hit { self.save(&list); }
    }

    pub fn save(&self, sessions: &[SessionMeta]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let clean: Vec<SessionMeta> = sessions.iter().enumerate().map(|(i, s)| {
            let mut c = s.clone();
            if !["ssh", "mosh", "et", "local"].contains(&c.transport.as_str()) { c.transport = "ssh".into(); }
            if !["control", "plain", "local"].contains(&c.mode.as_str()) { c.mode = "plain".into(); }
            c.tmux_path = safe_tmux_path(&c.tmux_path);
            c.socket_name = safe_socket_name(&c.socket_name);
            c.recovery_windows = clean_recovery_windows(&c.recovery_windows);
            if c.title.is_none() { c.title = Some(c.session.clone()); }
            c.order = i as i64;
            c
        }).collect();
        if let Ok(json) = serde_json::to_string_pretty(&clean) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The persisted tmux path/version must survive load with BOTH camelCase (new) and snake_case
    // (legacy) keys — the mismatch that dropped them caused a re-probe -> wrong socket -> couldn't
    // reattach existing sessions.
    #[test]
    fn loads_camel_and_legacy_snake_case() {
        let camel: SessionMeta = serde_json::from_str(
            r#"{"id":"1","host":"me@h","session":"dt-1","mode":"control",
                "tmuxPath":"/home/u/.local/bin/tmux","tmuxVersion":[3,7]}"#,
        ).unwrap();
        assert_eq!(camel.tmux_path.as_deref(), Some("/home/u/.local/bin/tmux"));
        assert_eq!(camel.tmux_version, Some((3, 7)));
        assert!(!camel.archived, "legacy rows remain in the active list");

        let legacy: SessionMeta = serde_json::from_str(
            r#"{"id":"1","host":"me@h","session":"dt-1","mode":"control",
                "tmux_path":"/home/u/.local/bin/tmux","tmux_version":[3,7]}"#,
        ).unwrap();
        assert_eq!(legacy.tmux_path.as_deref(), Some("/home/u/.local/bin/tmux"));
        assert_eq!(legacy.tmux_version, Some((3, 7)));
    }

    // We serialize camelCase so the renderer reads meta.tmuxPath / meta.tmuxVersion directly.
    #[test]
    fn serializes_camel_case() {
        let m = SessionMeta {
            id: "1".into(), host: "me@h".into(), session: "dt-1".into(),
            transport: "ssh".into(), mode: "control".into(),
            tmux_path: Some("/t".into()), tmux_version: Some((3, 7)),
            socket_name: None,
            title: Some("x".into()), order: 0, attach_ok: false,
            color: None, last_tab: None, tab_order: vec![], tab_colors: Default::default(),
            archived: false, archived_at: None,
            detached: false, recovery_tabs: vec![], restore_pending: false,
            recovery_windows: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"tmuxPath\""), "must serialize camelCase tmuxPath");
        assert!(json.contains("\"tmuxVersion\""));
        assert!(!json.contains("tmux_path"), "must NOT emit snake_case");
    }

    // TC-SS-L1 a LOCAL session row survives save/load. Local sessions are persisted now (§5.3b)
    // because their tmux server outlives the app, so the row is the only way back to it. Two guards
    // that both silently dropped or corrupted such rows before:
    //   - load() ran parse_host on every row, and a local row's host is legitimately EMPTY -> every
    //     persisted local session vanished on restart.
    //   - load()/save() clamped mode to control|plain, which would rewrite the no-tmux "local" mode.
    #[test]
    fn tc_ss_l1_local_rows_round_trip() {
        let dir = std::env::temp_dir().join(format!("dt-store-local-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = SessionStore::new(dir.join("sessions.json"));
        let mk = |id: &str, mode: &str| SessionMeta {
            id: id.into(), host: String::new(), session: format!("dt-{id}"),
            transport: "local".into(), mode: mode.into(),
            tmux_path: Some("/opt/homebrew/bin/tmux".into()), tmux_version: Some((3, 6)),
            socket_name: None,
            title: Some("local".into()), order: 0, attach_ok: false,
            color: None, last_tab: None, tab_order: vec![], tab_colors: Default::default(),
            archived: false, archived_at: None,
            detached: false, recovery_tabs: vec![], restore_pending: false,
            recovery_windows: vec![],
        };
        store.save(&[mk("l1", "control"), mk("l2", "plain"), mk("l3", "local")]);
        let loaded = store.load();
        assert_eq!(loaded.len(), 3, "all three local rows survive (an empty host is valid): {loaded:?}");
        for (i, want_mode) in ["control", "plain", "local"].iter().enumerate() {
            assert_eq!(loaded[i].mode, *want_mode, "mode preserved, not clamped");
            assert_eq!(loaded[i].transport, "local", "transport preserved");
            assert!(loaded[i].host.is_empty());
            assert_eq!(loaded[i].tmux_path.as_deref(), Some("/opt/homebrew/bin/tmux"));
            assert_eq!(loaded[i].tmux_version, Some((3, 6)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // TC-SS-L2 a malformed row is still rejected: 'local' transport WITH a host is contradictory
    // (which machine?), and a remote row with an unparseable host is dropped as before. Local mode
    // must not become a hole that smuggles an unvalidated host into argv construction.
    #[test]
    fn tc_ss_l2_malformed_rows_rejected() {
        let dir = std::env::temp_dir().join(format!("dt-store-badlocal-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sessions.json");
        std::fs::write(&path, r#"[
            {"id":"bad1","host":"evil.example","session":"dt-1","transport":"local","mode":"control"},
            {"id":"bad2","host":"-flag","session":"dt-2","transport":"ssh","mode":"control"},
            {"id":"bad3","host":"","session":"a;b","transport":"local","mode":"control"},
            {"id":"good","host":"","session":"dt-4","transport":"local","mode":"control"}
        ]"#).unwrap();
        let loaded = SessionStore::new(path).load();
        assert_eq!(loaded.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["good"],
            "only the well-formed local row loads; got {loaded:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // set_attach_ok flips ONE row's cache-confidence flag and leaves everything else (including
    // order and the other rows) untouched. create_session keys its re-probe decision off this, so a
    // write that clobbered a sibling row would re-probe the wrong session.
    #[test]
    fn set_attach_ok_touches_only_the_named_row() {
        let dir = std::env::temp_dir().join(format!("dt-store-attachok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = SessionStore::new(dir.join("sessions.json"));
        let mk = |id: &str, order: i64| SessionMeta {
            id: id.into(), host: "me@h".into(), session: format!("dt-{id}"),
            transport: "ssh".into(), mode: "control".into(),
            tmux_path: Some("/usr/bin/tmux".into()), tmux_version: Some((3, 7)),
            socket_name: None,
            title: Some(id.into()), order, attach_ok: false,
            color: None, last_tab: None, tab_order: vec![], tab_colors: Default::default(),
            archived: false, archived_at: None,
            detached: false, recovery_tabs: vec![], restore_pending: false,
            recovery_windows: vec![],
        };
        store.save(&[mk("a", 0), mk("b", 1)]);
        assert!(store.load().iter().all(|s| !s.attach_ok), "fresh rows start unproven");

        store.set_attach_ok("b", true);
        let loaded = store.load();
        assert_eq!(loaded.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(!loaded.iter().find(|s| s.id == "a").unwrap().attach_ok);
        assert!(loaded.iter().find(|s| s.id == "b").unwrap().attach_ok);
        // The cached tmux path must survive the flag write — it's what the flag vouches for.
        assert_eq!(loaded.iter().find(|s| s.id == "b").unwrap().tmux_path.as_deref(), Some("/usr/bin/tmux"));

        store.set_attach_ok("b", false);
        assert!(store.load().iter().all(|s| !s.attach_ok));
        store.set_attach_ok("nope", true);   // unknown id: no-op, no panic
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_round_trip_preserves_tmux_and_tab_state() {
        let dir = std::env::temp_dir().join(format!("dt-store-history-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = SessionStore::new(dir.join("sessions.json"));
        let original = SessionMeta {
            id: "resume-me".into(), host: "me@h".into(), session: "dt-resume".into(),
            transport: "ssh".into(), mode: "control".into(),
            tmux_path: Some("/usr/bin/tmux".into()), tmux_version: Some((3, 7)),
            title: Some("work".into()), order: 0, attach_ok: true,
            color: None, last_tab: Some("@2".into()), tab_order: vec!["@1".into(), "@2".into()],
            tab_colors: Default::default(), archived: false, archived_at: None,
            detached: false, recovery_tabs: vec![], restore_pending: false,
            socket_name: None, recovery_windows: vec![],
        };
        store.save(&[original]);
        assert!(store.set_archived("resume-me", true, Some(1234)));
        let archived = store.load().remove(0);
        assert!(archived.archived);
        assert_eq!(archived.archived_at, Some(1234));
        assert_eq!(archived.tmux_path.as_deref(), Some("/usr/bin/tmux"));
        assert_eq!(archived.last_tab.as_deref(), Some("@2"));
        assert_eq!(archived.tab_order, vec!["@1".to_string(), "@2".to_string()]);

        assert!(store.set_archived("resume-me", false, None));
        let resumed = store.load().remove(0);
        assert!(!resumed.archived);
        assert_eq!(resumed.archived_at, None);
        assert!(resumed.attach_ok, "resume must keep the proven tmux socket metadata");
        assert!(!store.set_archived("missing", true, Some(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // §20: order + customization survive a save/load round-trip, and load is order-sorted.
    #[test]
    fn persists_order_and_customization() {
        let dir = std::env::temp_dir().join(format!("dt-store-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sessions.json");
        let store = SessionStore::new(path.clone());
        let mk = |id: &str, order: i64, color: Option<&str>| SessionMeta {
            id: id.into(), host: "me@h".into(), session: format!("dt-{id}"),
            transport: "ssh".into(), mode: "control".into(),
            tmux_path: None, tmux_version: None, title: Some(id.into()), order,
            socket_name: None,
            attach_ok: false, color: color.map(String::from), last_tab: Some("@2".into()),
            tab_order: vec!["@2".into(), "@0".into()],
            tab_colors: [("@0".to_string(), "#89b4fa".to_string())].into_iter().collect(),
            archived: false, archived_at: None,
            detached: false, recovery_tabs: vec![], restore_pending: false,
            recovery_windows: vec![],
        };
        // save() assigns order by array position; load() returns in that order.
        store.save(&[mk("a", 0, None), mk("b", 1, Some("#a6e3a1"))]);
        let loaded = store.load();
        assert_eq!(loaded.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
        let b = loaded.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.color.as_deref(), Some("#a6e3a1"));
        assert_eq!(b.last_tab.as_deref(), Some("@2"));
        assert_eq!(b.tab_order, vec!["@2".to_string(), "@0".to_string()]);
        assert_eq!(b.tab_colors.get("@0").map(String::as_str), Some("#89b4fa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imported_socket_and_recovery_recipe_are_validated_and_persisted() {
        let dir = std::env::temp_dir().join(format!("buoy-store-recovery-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sessions.json");
        std::fs::write(&path, r#"[{
          "id":"imported","host":"me@host","session":"work","transport":"ssh",
          "mode":"control","socketName":"default",
          "recoveryWindows":[
            {"name":"agent\n","cwd":"/tmp/project","command":"codex\t","active":true},
            {"name":"logs","cwd":"/var/log","command":"zsh","active":true},
            {"name":"bad","cwd":"relative/path","command":"sh","active":false}
          ]
        }]"#).unwrap();
        let store = SessionStore::new(path);
        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        let session = &loaded[0];
        assert_eq!(session.socket_name.as_deref(), Some("default"));
        assert_eq!(session.recovery_windows.len(), 2, "relative cwd is discarded");
        assert_eq!(session.recovery_windows[0].name, "agent");
        assert_eq!(session.recovery_windows[0].command, "codex");
        assert!(session.recovery_windows[0].active);
        assert!(!session.recovery_windows[1].active, "only one active window survives");

        store.set_recovery_windows("imported", &[
            RecoveryWindow { name: "next".into(), cwd: "/opt/work".into(),
                command: "claude".into(), active: true },
        ]);
        let reloaded = store.load();
        assert_eq!(reloaded[0].recovery_windows[0].cwd, "/opt/work");
        assert_eq!(reloaded[0].recovery_windows[0].command, "claude");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
