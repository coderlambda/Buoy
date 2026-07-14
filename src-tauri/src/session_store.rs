//! Disk-backed session list (DESIGN.md §5.2). Port of src/main/sessionStore.js. The persisted
//! file is UNTRUSTED: re-validate host/session on load and drop invalid entries.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::validation::{parse_host, validate_session};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub host: String,
    pub session: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub tmux_path: Option<String>,
    #[serde(default)]
    pub tmux_version: Option<(u32, u32)>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub order: i64,
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
