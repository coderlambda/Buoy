//! WindowRegistry: single source of truth for a control-mode session's window/pane topology
//! (DESIGN.md §12/§14). Port of src/main/windowRegistry.js. Reconciles against the authoritative
//! `list-panes -s` listing and returns the exact diff; pure data structure, no IO.

use std::collections::{BTreeMap, BTreeSet};

/// One parsed row of the topology listing.
#[derive(Debug, Clone)]
pub struct PaneRow {
    pub win: String,
    pub pane: String,
    pub pane_active: bool,
    pub win_active: bool,
    pub name: String,
}

#[derive(Debug, Clone)]
struct WinState {
    name: String,
    panes: BTreeSet<String>,
    active_pane: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub renamed: Vec<(String, String)>, // (win, name)
    pub active_changed: bool,
    pub active: Option<String>,
    pub newly_mapped_panes: Vec<String>,
}

pub struct WindowRegistry {
    // insertion-ordered windows: keep a Vec of ids for `order`, plus a map for state.
    order: Vec<String>,
    windows: BTreeMap<String, WinState>,
    pane_to_win: BTreeMap<String, String>,
    pub active_window: Option<String>,
}

impl Default for WindowRegistry {
    fn default() -> Self { Self::new() }
}

impl WindowRegistry {
    pub fn new() -> Self {
        WindowRegistry {
            order: Vec::new(),
            windows: BTreeMap::new(),
            pane_to_win: BTreeMap::new(),
            active_window: None,
        }
    }

    pub fn win_for_pane(&self, pane: &str) -> Option<String> {
        self.pane_to_win.get(pane).cloned()
    }

    pub fn order(&self) -> Vec<String> {
        self.order.clone()
    }

    /// The current name of a window (as reconciled from tmux), if known.
    pub fn name_of(&self, win: &str) -> Option<String> {
        self.windows.get(win).map(|w| w.name.clone())
    }

    /// Reconcile against the authoritative listing rows; return the diff of what changed.
    pub fn reconcile(&mut self, rows: &[PaneRow]) -> Diff {
        let prev_wins: BTreeSet<String> = self.windows.keys().cloned().collect();
        let prev_names: BTreeMap<String, String> =
            self.windows.iter().map(|(w, s)| (w.clone(), s.name.clone())).collect();
        let prev_active = self.active_window.clone();
        let prev_panes: BTreeSet<String> = self.pane_to_win.keys().cloned().collect();

        // Rebuild from truth, preserving first-seen order.
        let mut next_order: Vec<String> = Vec::new();
        let mut next: BTreeMap<String, WinState> = BTreeMap::new();
        let mut next_active: Option<String> = None;

        for r in rows {
            if r.win.is_empty() || r.pane.is_empty() {
                continue;
            }
            let entry = next.entry(r.win.clone()).or_insert_with(|| {
                next_order.push(r.win.clone());
                WinState {
                    name: if r.name.is_empty() { r.win.clone() } else { r.name.clone() },
                    panes: BTreeSet::new(),
                    active_pane: None,
                }
            });
            entry.panes.insert(r.pane.clone());
            if r.pane_active {
                entry.active_pane = Some(r.pane.clone());
            }
            if !r.name.is_empty() {
                entry.name = r.name.clone();
            }
            if r.win_active {
                next_active = Some(r.win.clone());
            }
        }
        // Fallback active pane = first pane.
        for w in next.values_mut() {
            if w.active_pane.is_none() {
                if let Some(first) = w.panes.iter().next() {
                    w.active_pane = Some(first.clone());
                }
            }
        }

        // Commit.
        self.order = next_order;
        self.windows = next;
        self.pane_to_win = BTreeMap::new();
        for (win, w) in &self.windows {
            for p in &w.panes {
                self.pane_to_win.insert(p.clone(), win.clone());
            }
        }
        self.active_window = match next_active {
            Some(a) => Some(a),
            None => match &prev_active {
                Some(pa) if self.windows.contains_key(pa) => Some(pa.clone()),
                _ => self.order.first().cloned(),
            },
        };

        // Diff.
        let added: Vec<String> = self.order.iter().filter(|w| !prev_wins.contains(*w)).cloned().collect();
        let removed: Vec<String> = prev_wins.iter().filter(|w| !self.windows.contains_key(*w)).cloned().collect();
        let mut renamed: Vec<(String, String)> = Vec::new();
        for (win, w) in &self.windows {
            if prev_wins.contains(win) {
                if let Some(prev) = prev_names.get(win) {
                    if prev != &w.name {
                        renamed.push((win.clone(), w.name.clone()));
                    }
                }
            }
        }
        let newly_mapped_panes: Vec<String> =
            self.pane_to_win.keys().filter(|p| !prev_panes.contains(*p)).cloned().collect();

        Diff {
            added,
            removed,
            renamed,
            active_changed: self.active_window != prev_active,
            active: self.active_window.clone(),
            newly_mapped_panes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(win: &str, pane: &str, pane_active: bool, win_active: bool, name: &str) -> PaneRow {
        PaneRow {
            win: win.into(), pane: pane.into(),
            pane_active, win_active, name: name.into(),
        }
    }

    #[test]
    fn tc_wr1_initial_add() {
        let mut r = WindowRegistry::new();
        let d = r.reconcile(&[row("@0", "%0", true, false, "vim"), row("@1", "%1", true, true, "zsh")]);
        assert_eq!(d.added, vec!["@0", "@1"]);
        assert!(d.removed.is_empty());
        assert_eq!(d.active.as_deref(), Some("@1"));
        assert!(d.active_changed);
        assert_eq!(r.win_for_pane("%0").as_deref(), Some("@0"));
        assert_eq!(r.win_for_pane("%1").as_deref(), Some("@1"));
        let mut panes = d.newly_mapped_panes.clone();
        panes.sort();
        assert_eq!(panes, vec!["%0", "%1"]);
    }

    #[test]
    fn tc_wr_name_of_after_add() {
        // A window's name must be queryable right after it's added, so the backend can emit the
        // real title (not just "@N") for windows discovered on a reconnect/app-reopen.
        let mut r = WindowRegistry::new();
        r.reconcile(&[row("@0", "%0", true, true, "MyTab")]);
        assert_eq!(r.name_of("@0").as_deref(), Some("MyTab"));
        assert_eq!(r.name_of("@9"), None);
        // an empty tmux name falls back to the window id (never blank)
        r.reconcile(&[row("@0", "%0", true, true, ""), row("@1", "%1", false, false, "")]);
        assert_eq!(r.name_of("@1").as_deref(), Some("@1"));
    }

    #[test]
    fn tc_wr2_idempotent() {
        let mut r = WindowRegistry::new();
        let rows = [row("@0", "%0", true, true, "zsh")];
        r.reconcile(&rows);
        let d = r.reconcile(&rows);
        assert!(d.added.is_empty() && d.removed.is_empty() && d.renamed.is_empty());
        assert!(!d.active_changed);
        assert!(d.newly_mapped_panes.is_empty());
    }

    #[test]
    fn tc_wr3_new_window() {
        let mut r = WindowRegistry::new();
        r.reconcile(&[row("@0", "%0", true, true, "zsh")]);
        let d = r.reconcile(&[row("@0", "%0", true, false, "zsh"), row("@1", "%1", true, true, "zsh")]);
        assert_eq!(d.added, vec!["@1"]);
        assert_eq!(d.active.as_deref(), Some("@1"));
        assert!(d.active_changed);
        assert_eq!(d.newly_mapped_panes, vec!["%1"]);
    }

    #[test]
    fn tc_wr4_close_window() {
        let mut r = WindowRegistry::new();
        r.reconcile(&[row("@0", "%0", true, false, "zsh"), row("@1", "%1", true, true, "zsh")]);
        let d = r.reconcile(&[row("@0", "%0", true, true, "zsh")]);
        assert_eq!(d.removed, vec!["@1"]);
        assert_eq!(r.win_for_pane("%1"), None);
        assert_eq!(r.active_window.as_deref(), Some("@0"));
    }

    #[test]
    fn tc_wr5_rename() {
        let mut r = WindowRegistry::new();
        r.reconcile(&[row("@0", "%0", true, true, "zsh")]);
        let d = r.reconcile(&[row("@0", "%0", true, true, "node build")]);
        assert_eq!(d.renamed, vec![("@0".to_string(), "node build".to_string())]);
        assert!(d.added.is_empty());
    }

    #[test]
    fn tc_wr6_split_panes() {
        let mut r = WindowRegistry::new();
        r.reconcile(&[row("@0", "%0", true, true, "zsh"), row("@0", "%1", false, true, "zsh")]);
        assert_eq!(r.win_for_pane("%0").as_deref(), Some("@0"));
        assert_eq!(r.win_for_pane("%1").as_deref(), Some("@0"));
    }
}
