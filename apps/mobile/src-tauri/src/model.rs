use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub host: String,
    pub session: String,
    #[serde(default = "remote_kind")]
    pub kind: String,
    #[serde(default = "ssh_transport")]
    pub transport: String,
    #[serde(default = "control_mode")]
    pub mode: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "tmux_command")]
    pub tmux_path: String,
    #[serde(default)]
    pub tmux_version: Option<Vec<u32>>,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub last_tab: Option<String>,
    #[serde(default)]
    pub tab_order: Vec<String>,
    #[serde(default)]
    pub tab_colors: BTreeMap<String, String>,
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
}

impl SessionMeta {
    pub fn normalize(mut self) -> Self {
        self.kind = remote_kind();
        self.transport = ssh_transport();
        if self.mode != "plain" && self.mode != "control" {
            self.mode = control_mode();
        }
        if self.title.trim().is_empty() {
            self.title = self.host.clone();
        }
        if self.tmux_path.trim().is_empty() {
            self.tmux_path = tmux_command();
        }
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateArgs {
    pub id: Option<String>,
    pub host: String,
    pub session: Option<String>,
    pub title: Option<String>,
    pub mode: Option<String>,
    pub tmux_path: Option<String>,
    pub tmux_version: Option<Vec<u32>>,
    pub ssh_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub remote: u16,
    pub local: u16,
    #[serde(default)]
    pub active: bool,
}

pub fn remote_kind() -> String {
    "remote".into()
}

pub fn ssh_transport() -> String {
    "ssh".into()
}

pub fn control_mode() -> String {
    "control".into()
}

pub fn tmux_command() -> String {
    "tmux".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_sessions_are_normalized_to_mobile_safe_values() {
        let session = SessionMeta {
            id: "1".into(),
            host: "alice@vpn-host".into(),
            session: "dt-mobile".into(),
            kind: "local".into(),
            transport: "local".into(),
            mode: "unexpected".into(),
            title: String::new(),
            tmux_path: String::new(),
            tmux_version: None,
            order: 0,
            color: None,
            last_tab: None,
            tab_order: Vec::new(),
            tab_colors: BTreeMap::new(),
            archived: false,
            archived_at: None,
            detached: false,
            recovery_tabs: Vec::new(),
            restore_pending: false,
        }
        .normalize();
        assert_eq!(session.kind, "remote");
        assert_eq!(session.transport, "ssh");
        assert_eq!(session.mode, "control");
        assert_eq!(session.title, "alice@vpn-host");
        assert_eq!(session.tmux_path, "tmux");
    }
}
