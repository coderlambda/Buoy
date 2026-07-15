//! Disk-backed session list (DESIGN.md §5.2). Port of src/main/sessionStore.js. The persisted
//! file is UNTRUSTED: re-validate host/session on load and drop invalid entries.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::validation::{parse_host, validate_session};

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
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub order: i64,
    // §20: sidebar/tab customization, all persisted so they survive restart.
    #[serde(default)]
    pub color: Option<String>,                    // project accent color (hex like "#89b4fa")
    #[serde(default)]
    pub last_tab: Option<String>,                 // last-active tmux window id ("@N")
    #[serde(default)]
    pub tab_order: Vec<String>,                   // custom tab order (window ids); missing -> tmux order
    #[serde(default)]
    pub tab_colors: std::collections::BTreeMap<String, String>,   // window id -> accent color
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
            if parse_host(&e.host).is_err() { continue; }
            if !["ssh", "mosh", "et"].contains(&e.transport.as_str()) {
                e.transport = "ssh".into();
            }
            if e.mode != "control" { e.mode = "plain".into(); }
            e.tmux_path = safe_tmux_path(&e.tmux_path);
            if e.title.is_none() { e.title = Some(e.session.clone()); }
            out.push(e);
        }
        out.sort_by_key(|e| e.order);
        out
    }

    pub fn save(&self, sessions: &[SessionMeta]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let clean: Vec<SessionMeta> = sessions.iter().enumerate().map(|(i, s)| {
            let mut c = s.clone();
            if !["ssh", "mosh", "et"].contains(&c.transport.as_str()) { c.transport = "ssh".into(); }
            if c.mode != "control" { c.mode = "plain".into(); }
            c.tmux_path = safe_tmux_path(&c.tmux_path);
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
            title: Some("x".into()), order: 0,
            color: None, last_tab: None, tab_order: vec![], tab_colors: Default::default(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"tmuxPath\""), "must serialize camelCase tmuxPath");
        assert!(json.contains("\"tmuxVersion\""));
        assert!(!json.contains("tmux_path"), "must NOT emit snake_case");
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
            color: color.map(String::from), last_tab: Some("@2".into()),
            tab_order: vec!["@2".into(), "@0".into()],
            tab_colors: [("@0".to_string(), "#89b4fa".to_string())].into_iter().collect(),
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
}
