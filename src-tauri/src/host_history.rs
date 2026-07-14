//! Persisted history of connected hosts (for the new-session dialog's host dropdown).
//! Stored as a JSON array in hosts.json in the app data dir, most-recent-first, deduped, capped.
//! A host is only remembered if it parses/validates (untrusted-safe).

use std::path::PathBuf;
use std::sync::Mutex;

use crate::validation::parse_host;

const CAP: usize = 30;

pub struct HostHistory {
    hosts: Mutex<Vec<String>>,
    path: PathBuf,
}

impl HostHistory {
    /// Load history from `path` (missing/corrupt -> empty).
    pub fn load(path: PathBuf) -> Self {
        let hosts = std::fs::read_to_string(&path).ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        HostHistory { hosts: Mutex::new(hosts), path }
    }

    pub fn list(&self) -> Vec<String> {
        self.hosts.lock().unwrap().clone()
    }

    /// Record a host at the front (most-recent-first), deduped and capped. Ignores invalid hosts.
    pub fn remember(&self, host: &str) {
        let host = host.trim();
        if host.is_empty() || parse_host(host).is_err() {
            return;
        }
        let mut h = self.hosts.lock().unwrap();
        h.retain(|x| x != host);      // move-to-front: drop any existing copy
        h.insert(0, host.to_string());
        h.truncate(CAP);
        let snapshot = h.clone();
        drop(h);
        self.save(&snapshot);
    }

    fn save(&self, hosts: &[String]) {
        if let Some(dir) = self.path.parent() { let _ = std::fs::create_dir_all(dir); }
        if let Ok(json) = serde_json::to_string_pretty(hosts) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() { let _ = std::fs::rename(&tmp, &self.path); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        std::env::temp_dir().join(format!("dt_hosts_test_{}.json", std::process::id()))
    }

    #[test]
    fn remembers_most_recent_first_deduped_and_validates() {
        let path = tmp_path();
        let _ = std::fs::remove_file(&path);
        let hh = HostHistory::load(path.clone());
        assert!(hh.list().is_empty());

        hh.remember("me@host1");
        hh.remember("me@host2:22");
        hh.remember("me@host1");         // re-remember -> moves to front, no dup
        assert_eq!(hh.list(), vec!["me@host1", "me@host2:22"]);

        hh.remember("");                 // ignored
        hh.remember("-bad;host");        // invalid -> ignored
        assert_eq!(hh.list(), vec!["me@host1", "me@host2:22"]);

        // survives reload from disk
        let hh2 = HostHistory::load(path.clone());
        assert_eq!(hh2.list(), vec!["me@host1", "me@host2:22"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn caps_history() {
        let path = std::env::temp_dir().join(format!("dt_hosts_cap_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let hh = HostHistory::load(path.clone());
        for i in 0..40 { hh.remember(&format!("h{}", i)); }
        assert_eq!(hh.list().len(), CAP, "history is capped");
        assert_eq!(hh.list()[0], "h39", "most recent first");
        let _ = std::fs::remove_file(&path);
    }
}
