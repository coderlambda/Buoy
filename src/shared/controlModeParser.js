'use strict';
// Pure parser for the tmux control-mode (`tmux -CC`) protocol (DESIGN.md §12).
// Feed it stream chunks; it emits structured events via a callback. No I/O, no state beyond
// line buffering + %begin/%end correlation — so it is fully unit-testable against captured
// real bytes from a live tmux 3.5a.
//
// Event shapes (the `type` field):
//   { type:'output', pane:'%0', data:'<raw bytes, un-escaped>' }
//   { type:'window-add', window:'@1' }
//   { type:'window-close', window:'@1' }
//   { type:'window-renamed', window:'@1', name:'zsh' }
//   { type:'window-pane-changed', window:'@1', pane:'%2' }
//   { type:'session-changed', session:'$0', name:'spec' }
//   { type:'session-window-changed', session:'$0', window:'@1' }
//   { type:'layout-change', window:'@1', layout:'419a,80x24,0,0[...]' }
//   { type:'reply', cmd:'272', ok:true|false, body:['line', ...] }   // from %begin..%end/%error
//   { type:'exit', reason:'<maybe empty>' }
//   { type:'unknown', line:'%something ...' }                        // forward-compatible

// Un-escape tmux %output payload: octal escapes like \033 \015 \012 back to raw bytes,
// plus \\ -> \ . tmux emits 3-digit octal after a backslash.
function unescapeOutput(s) {
  let out = '';
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '\\' && i + 1 < s.length) {
      const n = s[i + 1];
      if (n === '\\') { out += '\\'; i += 1; continue; }
      const oct = s.slice(i + 1, i + 4);
      if (/^[0-7]{3}$/.test(oct)) {
        out += String.fromCharCode(parseInt(oct, 8));
        i += 3;
        continue;
      }
      out += '\\';
    } else {
      out += s[i];
    }
  }
  return out;
}

// Leading DCS control-mode marker: ESC P 1000 p (ESC optional); trailing ST: ESC \ .
const MARKER_RE = /^\x1b?P1000p/;      // eslint-disable-line no-control-regex
const ST_RE = /\x1b\\$/;               // eslint-disable-line no-control-regex

class ControlModeParser {
  constructor(onEvent) {
    this.onEvent = onEvent || (() => {});
    this.buf = '';
    this.inReply = null;   // { cmd, lines: [] } while between %begin and %end/%error
  }

  write(chunk) {
    this.buf += chunk;
    let nl;
    while ((nl = this.buf.indexOf('\n')) !== -1) {
      let line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.endsWith('\r')) line = line.slice(0, -1);
      this._line(line);
    }
  }

  _emit(ev) { this.onEvent(ev); }

  _line(line) {
    line = line.replace(MARKER_RE, '').replace(ST_RE, '');
    if (line === '') return;

    // Lines BETWEEN %begin and %end/%error are the command's REPLY BODY — verbatim, even if
    // they start with '%' (e.g. `display-message -p '#{pane_id}'` replies with a line like
    // "%34"). Only %end/%error/%output(-flow) are dispatched while inside a reply block; all
    // else is body text. (Treating "%34" as a control line silently broke the pane query.)
    if (this.inReply) {
      if (line === '%end' || line.startsWith('%end ') || line === '%error' || line.startsWith('%error ')) {
        this._emit({ type: 'reply', cmd: this.inReply.cmd, ok: line.startsWith('%end'), body: this.inReply.lines });
        this.inReply = null;
        return;
      }
      this.inReply.lines.push(line);
      return;
    }

    if (line[0] !== '%') {
      // Outside a reply block, a non-% line is not meaningful on its own.
      return;
    }

    const sp = line.indexOf(' ');
    const kw = sp === -1 ? line : line.slice(0, sp);
    const rest = sp === -1 ? '' : line.slice(sp + 1);

    switch (kw) {
      case '%output': {
        const m = /^(%\d+)\s([\s\S]*)$/.exec(rest);
        if (m) this._emit({ type: 'output', pane: m[1], data: unescapeOutput(m[2]) });
        return;
      }
      case '%begin': {
        const cmd = rest.split(' ')[1];   // <ts> <cmd#> <flags>
        this.inReply = { cmd, lines: [] };
        this._emit({ type: 'begin', cmd });
        return;
      }
      case '%end':
      case '%error': {
        if (this.inReply) {
          this._emit({ type: 'reply', cmd: this.inReply.cmd, ok: kw === '%end', body: this.inReply.lines });
          this.inReply = null;
        }
        return;
      }
      case '%window-add':   return this._emit({ type: 'window-add', window: rest.trim() });
      case '%window-close': return this._emit({ type: 'window-close', window: rest.trim() });
      case '%unlinked-window-close': return this._emit({ type: 'window-close', window: rest.trim() });
      case '%window-renamed': {
        const i = rest.indexOf(' ');
        return this._emit({ type: 'window-renamed', window: rest.slice(0, i), name: rest.slice(i + 1) });
      }
      case '%window-pane-changed': {
        const [w, p] = rest.split(' ');
        return this._emit({ type: 'window-pane-changed', window: w, pane: p });
      }
      case '%session-changed': {
        const i = rest.indexOf(' ');
        return this._emit({ type: 'session-changed', session: rest.slice(0, i), name: rest.slice(i + 1) });
      }
      case '%session-window-changed': {
        const [s, w] = rest.split(' ');
        return this._emit({ type: 'session-window-changed', session: s, window: w });
      }
      case '%layout-change': {
        const i = rest.indexOf(' ');
        return this._emit({ type: 'layout-change', window: rest.slice(0, i), layout: rest.slice(i + 1) });
      }
      case '%sessions-changed': return this._emit({ type: 'sessions-changed' });
      case '%exit': return this._emit({ type: 'exit', reason: rest.trim() });
      default: return this._emit({ type: 'unknown', line });
    }
  }
}

// Parse a tmux layout string into a split tree (DESIGN.md §12).
// "<checksum>,<cell>" where <cell> is:
//   WxH,X,Y,<paneId>                leaf pane
//   WxH,X,Y[<cell>,<cell>,...]      left-right split
//   WxH,X,Y{<cell>,<cell>,...}      top-bottom split
function parseLayout(layout) {
  const comma = layout.indexOf(',');
  const body = comma === -1 ? layout : layout.slice(comma + 1);
  const p = { s: body, i: 0 };

  function parseCell() {
    const m = /^(\d+)x(\d+),(\d+),(\d+)/.exec(p.s.slice(p.i));
    if (!m) throw new Error('bad layout at ' + p.i);
    p.i += m[0].length;
    const node = { w: +m[1], h: +m[2], x: +m[3], y: +m[4] };
    const c = p.s[p.i];
    if (c === '[' || c === '{') {
      const close = c === '[' ? ']' : '}';
      node.split = c === '[' ? 'lr' : 'tb';
      node.children = [];
      p.i += 1;
      for (;;) {
        node.children.push(parseCell());
        if (p.s[p.i] === ',') { p.i += 1; continue; }
        if (p.s[p.i] === close) { p.i += 1; break; }
        break;
      }
    } else if (c === ',') {
      p.i += 1;
      const pm = /^\d+/.exec(p.s.slice(p.i));
      if (pm) { node.pane = '%' + pm[0]; p.i += pm[0].length; }
    }
    return node;
  }
  return parseCell();
}

function layoutPanes(node, acc = []) {
  if (!node) return acc;
  if (node.pane) acc.push(node.pane);
  if (node.children) node.children.forEach((c) => layoutPanes(c, acc));
  return acc;
}

module.exports = { ControlModeParser, unescapeOutput, parseLayout, layoutPanes };
