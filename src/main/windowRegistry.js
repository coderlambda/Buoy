'use strict';
// WindowRegistry: the single source of truth for a control-mode session's window/pane topology
// (DESIGN.md §12/§14). tmux emits many overlapping, order-varying signals about windows and
// panes (%window-add, %session-window-changed, %layout-change, %window-pane-changed) — none of
// which alone gives a complete, consistent picture, and whose interleaving caused output/input
// to route to the wrong tab. Rather than react to each signal ad hoc, the backend treats the
// authoritative `list-panes -s` listing as truth and RECONCILES this registry against it. Every
// topology change just triggers a reconcile; this object computes the diff so the backend emits
// exactly the window add/close/rename/active events that actually changed (idempotent: a second
// reconcile with the same rows yields an empty diff).
//
// It maps pane -> window (for routing %output) and tracks the active window (for addressing
// input/capture). It holds NO tmux/IO — it is a plain data structure, fully testable.

class WindowRegistry {
  constructor() {
    this.windows = new Map();   // winId '@N' -> { name, panes: Set<paneId>, activePane }
    this.paneToWin = new Map(); // paneId '%N' -> winId '@N'
    this.activeWindow = null;   // '@N' | null
  }

  winForPane(pane) { return this.paneToWin.get(pane) || null; }
  has(win) { return this.windows.has(win); }
  get order() { return [...this.windows.keys()]; }
  isEmpty() { return this.windows.size === 0; }

  // Reconcile against the authoritative rows from
  //   list-panes -s -F '#{window_id} #{pane_id} #{pane_active} #{window_active} #{window_name}'
  // parsed to [{ win, pane, paneActive:bool, winActive:bool, name }].
  // Returns a diff describing what changed so the caller can emit the corresponding events and
  // flush any output buffered for newly-mapped panes:
  //   { added:[win], removed:[win], renamed:[{win,name}], activeChanged:bool,
  //     active:win|null, newlyMappedPanes:[pane] }
  reconcile(rows) {
    const prevWins = new Set(this.windows.keys());
    const prevNames = new Map([...this.windows].map(([w, s]) => [w, s.name]));
    const prevActive = this.activeWindow;
    const prevPanes = new Set(this.paneToWin.keys());

    // Rebuild from truth.
    const nextWindows = new Map();
    let nextActive = null;
    for (const r of rows) {
      if (!r || !r.win || !r.pane) continue;
      let w = nextWindows.get(r.win);
      if (!w) { w = { name: r.name || r.win, panes: new Set(), activePane: null }; nextWindows.set(r.win, w); }
      w.panes.add(r.pane);
      if (r.paneActive) w.activePane = r.pane;
      if (r.winActive) nextActive = r.win;
      if (r.name != null) w.name = r.name;
    }
    // A window with no explicit active pane: fall back to its first pane.
    for (const w of nextWindows.values()) if (!w.activePane && w.panes.size) w.activePane = [...w.panes][0];

    // Commit.
    this.windows = nextWindows;
    this.paneToWin = new Map();
    for (const [win, w] of nextWindows) for (const p of w.panes) this.paneToWin.set(p, win);
    this.activeWindow = nextActive != null ? nextActive
      : (nextWindows.has(prevActive) ? prevActive : (this.order[0] || null));

    // Diff.
    const added = [...nextWindows.keys()].filter((w) => !prevWins.has(w));
    const removed = [...prevWins].filter((w) => !nextWindows.has(w));
    const renamed = [];
    for (const [win, w] of nextWindows) {
      if (prevWins.has(win) && prevNames.get(win) !== w.name) renamed.push({ win, name: w.name });
    }
    const newlyMappedPanes = [...this.paneToWin.keys()].filter((p) => !prevPanes.has(p));
    return {
      added, removed, renamed,
      activeChanged: this.activeWindow !== prevActive,
      active: this.activeWindow,
      newlyMappedPanes,
    };
  }
}

module.exports = { WindowRegistry };
