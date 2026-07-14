'use strict';
// Minimal plugin framework (DESIGN.md §13). The first extension point is "link matchers":
// a plugin contributes a regex + an onClick handler, and matched substrings in the terminal
// become clickable, invoking the handler. URL and path detection are themselves built-in
// plugins using this same API — so third-party matchers (ticket ids, PR numbers, custom
// schemes) plug in identically.
//
// This module is PURE (no xterm, no Electron): it owns the registry and the matching engine,
// so it is fully unit-testable. The renderer wires the results into an xterm LinkProvider.

// A link plugin:
//   { name: string, priority?: number, regex: RegExp (global), onClick(text, ctx) }
// - regex MUST be global (/g); a fresh lastIndex is used per line.
// - onClick receives the matched text and a ctx (provided by the host).
// - priority: higher wins when two plugins match overlapping ranges (default 0).

class PluginRegistry {
  constructor() {
    this.linkPlugins = [];
    this.tabKinds = new Map();   // kind -> { create(spec, ctx) -> TabContent }
  }

  // --- Tab-kind extension point (§14/§15): a tab is polymorphic. A tab-kind provider knows
  // how to CREATE the content for tabs of its kind. 'terminal' is built in; future kinds
  // (markdown, browser, ...) register the same way — no renderer changes needed.
  //   provider: { kind:string, create(spec, ctx) -> TabContent }
  //   TabContent: { mount(el), dispose(), onData?(d), resize?(c,r), focus?() }
  registerTabKind(provider) {
    if (!provider || !provider.kind || typeof provider.create !== 'function') {
      throw new Error('tab-kind provider requires { kind:string, create:fn }');
    }
    this.tabKinds.set(provider.kind, provider);
    return () => { this.tabKinds.delete(provider.kind); };
  }

  createTabContent(kind, spec, ctx) {
    const provider = this.tabKinds.get(kind);
    if (!provider) throw new Error(`no tab-kind registered for "${kind}"`);
    return provider.create(spec, ctx);
  }

  hasTabKind(kind) { return this.tabKinds.has(kind); }

  registerLink(plugin) {
    if (!plugin || typeof plugin.onClick !== 'function' || !(plugin.regex instanceof RegExp)) {
      throw new Error('link plugin requires { regex:RegExp, onClick:fn }');
    }
    if (!plugin.regex.global) throw new Error(`link plugin "${plugin.name}" regex must be global (/g)`);
    const entry = { priority: 0, name: 'anon', ...plugin };
    this.linkPlugins.push(entry);
    // higher priority first, so overlap resolution prefers it
    this.linkPlugins.sort((a, b) => b.priority - a.priority);
    return () => { this.linkPlugins = this.linkPlugins.filter((p) => p !== entry); };
  }

  // Find all non-overlapping link matches in a single line of text.
  // Returns [{ start, end, text, plugin }] with 0-based [start,end) column indices.
  // Higher-priority plugins claim ranges first; lower ones can't overlap claimed ranges.
  findMatches(line) {
    const claimed = [];      // sorted, non-overlapping [start,end)
    const out = [];
    const overlaps = (s, e) => claimed.some((c) => s < c.end && e > c.start);
    for (const plugin of this.linkPlugins) {   // already priority-sorted
      plugin.regex.lastIndex = 0;
      let m;
      while ((m = plugin.regex.exec(line)) !== null) {
        const start = m.index;
        const end = start + m[0].length;
        if (m[0].length === 0) { plugin.regex.lastIndex++; continue; }  // avoid zero-width loop
        if (!overlaps(start, end)) {
          out.push({ start, end, text: m[0], plugin });
          claimed.push({ start, end });
        }
      }
    }
    out.sort((a, b) => a.start - b.start);
    return out;
  }
}

// UMD-lite: CommonJS for tests/main, global for the sandboxed renderer (<script> tag).
if (typeof module !== 'undefined' && module.exports) module.exports = { PluginRegistry };
if (typeof window !== 'undefined') window.DTPlugins = { PluginRegistry };
