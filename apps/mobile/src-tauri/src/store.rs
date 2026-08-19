use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model::{SessionMeta, TunnelStatus};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedData {
    #[serde(default)]
    sessions: Vec<SessionMeta>,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    last_active: Option<String>,
    #[serde(default)]
    known_hosts: BTreeMap<String, String>,
    #[serde(default)]
    tunnels: BTreeMap<String, Vec<TunnelStatus>>,
}

pub struct MobileStore {
    path: PathBuf,
    data: Mutex<PersistedData>,
}

impl MobileStore {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir).map_err(|error| error.to_string())?;
        let path = data_dir.join("mobile-state.json");
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<PersistedData>(&bytes)
                .map_err(|error| format!("invalid mobile state: {error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedData::default(),
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    fn mutate<T>(&self, update: impl FnOnce(&mut PersistedData) -> T) -> Result<T, String> {
        let mut data = self.data.lock().map_err(|_| "mobile store lock poisoned")?;
        let result = update(&mut data);
        self.save_locked(&data)?;
        Ok(result)
    }

    fn save_locked(&self, data: &PersistedData) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(data).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }

    pub fn sessions(&self) -> Vec<SessionMeta> {
        let mut sessions = self
            .data
            .lock()
            .map(|data| data.sessions.clone())
            .unwrap_or_default();
        sessions.sort_by_key(|session| session.order);
        sessions
            .into_iter()
            .map(SessionMeta::normalize)
            .filter(|session| {
                buoy_core::parse_ssh_target(&session.host)
                    .ok()
                    .is_some_and(|target| target.user.is_some())
                    && buoy_core::validate_session_name(&session.session).is_ok()
            })
            .collect()
    }

    pub fn session(&self, id: &str) -> Option<SessionMeta> {
        self.sessions().into_iter().find(|session| session.id == id)
    }

    pub fn upsert_session(&self, session: SessionMeta) -> Result<(), String> {
        self.mutate(|data| {
            data.sessions.retain(|candidate| candidate.id != session.id);
            data.sessions.push(session);
            data.sessions.sort_by_key(|candidate| candidate.order);
        })
    }

    pub fn remove_session(&self, id: &str) -> Result<(), String> {
        self.mutate(|data| {
            data.sessions.retain(|session| session.id != id);
            data.tunnels.remove(id);
            if data.last_active.as_deref() == Some(id) {
                data.last_active = None;
            }
        })
    }

    pub fn reorder_sessions(&self, ids: &[String]) -> Result<(), String> {
        self.mutate(|data| {
            for (order, id) in ids.iter().enumerate() {
                if let Some(session) = data.sessions.iter_mut().find(|session| &session.id == id) {
                    session.order = order as i64;
                }
            }
            data.sessions.sort_by_key(|session| session.order);
        })
    }

    pub fn update_session(
        &self,
        id: &str,
        update: impl FnOnce(&mut SessionMeta),
    ) -> Result<Option<SessionMeta>, String> {
        self.mutate(|data| {
            let session = data.sessions.iter_mut().find(|session| session.id == id)?;
            update(session);
            Some(session.clone())
        })
    }

    pub fn hosts(&self) -> Vec<String> {
        self.data
            .lock()
            .map(|data| data.hosts.clone())
            .unwrap_or_default()
    }

    pub fn remember_host(&self, host: &str) -> Result<(), String> {
        let host = host.to_string();
        self.mutate(|data| {
            data.hosts.retain(|candidate| candidate != &host);
            data.hosts.insert(0, host);
            data.hosts.truncate(50);
        })
    }

    pub fn last_active(&self) -> Option<String> {
        self.data
            .lock()
            .ok()
            .and_then(|data| data.last_active.clone())
    }

    pub fn set_last_active(&self, id: Option<String>) -> Result<(), String> {
        self.mutate(|data| data.last_active = id)
    }

    pub fn check_or_remember_host_key(
        &self,
        endpoint: &str,
        fingerprint: &str,
    ) -> Result<bool, String> {
        let endpoint = endpoint.to_string();
        let fingerprint = fingerprint.to_string();
        self.mutate(|data| match data.known_hosts.get(&endpoint) {
            Some(known) => known == &fingerprint,
            None => {
                data.known_hosts.insert(endpoint, fingerprint);
                true
            }
        })
    }

    pub fn tunnels(&self, id: &str) -> Vec<TunnelStatus> {
        self.data
            .lock()
            .ok()
            .and_then(|data| data.tunnels.get(id).cloned())
            .unwrap_or_default()
    }

    pub fn upsert_tunnel(&self, id: &str, tunnel: TunnelStatus) -> Result<(), String> {
        let id = id.to_string();
        self.mutate(|data| {
            let tunnels = data.tunnels.entry(id).or_default();
            tunnels.retain(|candidate| candidate.remote != tunnel.remote);
            tunnels.push(TunnelStatus {
                active: false,
                ..tunnel
            });
            tunnels.sort_by_key(|candidate| candidate.remote);
        })
    }

    pub fn remove_tunnel(&self, id: &str, remote: u16) -> Result<(), String> {
        self.mutate(|data| {
            if let Some(tunnels) = data.tunnels.get_mut(id) {
                tunnels.retain(|candidate| candidate.remote != remote);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionMeta;
    use std::collections::BTreeMap;

    fn session(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            host: "alice@vpn-host".into(),
            session: format!("dt-{id}"),
            kind: "remote".into(),
            transport: "ssh".into(),
            mode: "control".into(),
            title: id.into(),
            tmux_path: "tmux".into(),
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
    }

    #[test]
    fn persists_sessions_tofu_and_tunnels_without_credentials() {
        let root = std::env::temp_dir().join(format!(
            "buoy-mobile-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let store = MobileStore::load(&root).unwrap();
        store.upsert_session(session("one")).unwrap();
        assert!(
            store
                .check_or_remember_host_key("vpn-host:22", "SHA256:first")
                .unwrap()
        );
        assert!(
            store
                .check_or_remember_host_key("vpn-host:22", "SHA256:first")
                .unwrap()
        );
        assert!(
            !store
                .check_or_remember_host_key("vpn-host:22", "SHA256:changed")
                .unwrap()
        );
        store
            .upsert_tunnel(
                "one",
                TunnelStatus {
                    remote: 3000,
                    local: 45123,
                    active: true,
                },
            )
            .unwrap();

        let reloaded = MobileStore::load(&root).unwrap();
        assert_eq!(reloaded.sessions().len(), 1);
        assert_eq!(reloaded.tunnels("one")[0].remote, 3000);
        assert!(!reloaded.tunnels("one")[0].active);
        let json = fs::read_to_string(root.join("mobile-state.json")).unwrap();
        assert!(!json.to_ascii_lowercase().contains("password"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn archived_session_keeps_reattach_identity_and_tab_preferences() {
        let root = std::env::temp_dir().join(format!(
            "buoy-mobile-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let store = MobileStore::load(&root).unwrap();
        let mut saved = session("history");
        saved.last_tab = Some("@2".into());
        saved.tab_order = vec!["@1".into(), "@2".into()];
        saved.tmux_version = Some(vec![3, 7]);
        store.upsert_session(saved).unwrap();

        store.update_session("history", |session| {
            session.archived = true;
            session.archived_at = Some(1234);
        }).unwrap();
        let reloaded = MobileStore::load(&root).unwrap();
        let archived = reloaded.session("history").unwrap();
        assert!(archived.archived);
        assert_eq!(archived.archived_at, Some(1234));
        assert_eq!(archived.session, "dt-history");
        assert_eq!(archived.tmux_version, Some(vec![3, 7]));
        assert_eq!(archived.last_tab.as_deref(), Some("@2"));
        assert_eq!(archived.tab_order, vec!["@1".to_string(), "@2".to_string()]);
        let _ = fs::remove_dir_all(root);
    }
}
