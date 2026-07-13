'use strict';
// Built-in 'terminal' tab-kind (§14/§15). Wraps an xterm.js terminal as a polymorphic
// TabContent so the project/tab machinery can host it generically alongside future tab kinds
// (markdown, browser, ...). Only terminal tabs bind to the tmux backend; other kinds ignore
// onData/resize. This module is the reference implementation of the TabContent interface.
/* global Terminal, FitAddon */

// spec: { id, meta, linkProvider }  ctx: { input(bytes), ack(id,bytes), onReady?, ... }
// Returns a TabContent: { kind, mount(el), onData(d), resize(c,r), focus(), dispose(),
//                         readBuffer() (test hook), term (raw xterm) }.
function createTerminalTab(spec, ctx) {
  const term = new Terminal({
    fontFamily: 'Menlo, Consolas, monospace', fontSize: 13,
    theme: { background: '#1e1e2e', foreground: '#cdd6f4' }, scrollback: 5000,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  if (spec.linkProvider) term.registerLinkProvider(spec.linkProvider);

  let mounted = false;
  let el = null;
  const preOpen = [];   // bytes buffered until the xterm is opened

  // input up (gating is applied by the caller via ctx.input, which may buffer)
  term.onData((data) => ctx.input(data));

  return {
    kind: 'terminal',
    term,                      // raw handle (link provider, tests)
    get mounted() { return mounted; },

    mount(container) {
      if (mounted) { container.appendChild(el); return; }
      el = document.createElement('div');
      el.style.width = '100%'; el.style.height = '100%';
      container.appendChild(el);
      term.open(el);
      mounted = true;
      if (preOpen.length) { const b = preOpen.splice(0); b.forEach((d) => term.write(d)); }
      this.fit();
    },
    element() { return el; },

    onData(data) {
      if (!mounted) { preOpen.push(data); return; }
      term.write(data, () => { if (ctx.ack) ctx.ack(byteLen(data)); });
    },

    fit() { try { fit.fit(); } catch (_) {} return { cols: term.cols, rows: term.rows }; },
    resize(cols, rows) { try { term.resize(cols, rows); } catch (_) {} },
    focus() { try { term.focus(); } catch (_) {} },

    readBuffer() {   // test hook
      const buf = term.buffer.active; let out = '';
      for (let i = 0; i < buf.length; i++) { const l = buf.getLine(i); if (l) out += l.translateToString(true) + '\n'; }
      return out;
    },

    dispose() { try { term.dispose(); } catch (_) {} if (el && el.parentNode) el.parentNode.removeChild(el); },
  };
}

function byteLen(s) { return typeof TextEncoder !== 'undefined' ? new TextEncoder().encode(s).length : Buffer.byteLength(s); }

if (typeof module !== 'undefined' && module.exports) module.exports = { createTerminalTab };
if (typeof window !== 'undefined') window.DTTerminalTab = { createTerminalTab };
