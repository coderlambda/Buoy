'use strict';
// 'fileviewer' tab kind (DESIGN.md §16): renders a fetched remote/local file in-app as text,
// markdown, or image, with a Download-to-local button. It is APP-LOCAL — no tmux window — so the
// tab machinery must never send tmux window commands for it (gated on real @N ids elsewhere).
//
// spec: { id (session id), path, api }   ctx: { setStatus }
// The tab fetches its own content on mount (via api.readRemoteFile) so opening is one call.
/* global module */

// Tiered size caps: rendering cost, not the network, is the limit (§16). Over the render cap we
// show a download-only panel instead of freezing the webview.
const TEXT_RENDER_CAP = 1 * 1024 * 1024;   // text/markdown -> DOM
const IMAGE_RENDER_CAP = 5 * 1024 * 1024;  // image -> data: URL decode
// HTML gets a HIGHER cap than text even though it's also text: it goes to a real browser parser in
// an iframe (not our hand-rolled markdown -> DOM path), and the whole point of the feature is
// SELF-CONTAINED files, whose size is dominated by inlined base64 images/fonts. Exported notebooks
// and plot reports routinely land in the 2-5 MB range and render fine.
const HTML_RENDER_CAP = 5 * 1024 * 1024;

const IMAGE_EXT = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif',
  webp: 'image/webp', bmp: 'image/bmp', svg: 'image/svg+xml', ico: 'image/x-icon' };
const MD_EXT = { md: 1, markdown: 1, mdown: 1, mkd: 1 };
const HTML_EXT = { html: 1, htm: 1, xhtml: 1 };

function extOf(path) {
  const base = String(path).split('/').pop() || '';
  const dot = base.lastIndexOf('.');
  return dot > 0 ? base.slice(dot + 1).toLowerCase() : '';
}
function baseName(path) { return (String(path).split('/').pop() || 'file'); }

// base64 -> Uint8Array (bytes), and a UTF-8 decode for text.
function b64ToBytes(b64) {
  const bin = atob(b64);
  const arr = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
  return arr;
}
// Binary sniff: NUL byte or invalid UTF-8 => not text.
function looksBinary(bytes) {
  const n = Math.min(bytes.length, 4096);
  for (let i = 0; i < n; i++) if (bytes[i] === 0) return true;
  try { new TextDecoder('utf-8', { fatal: true }).decode(bytes.slice(0, n)); return false; }
  catch (_) { return true; }
}

// PURE render-decision (unit-tested): given path + reported size + bytes, decide what to render.
// Returns { mode: 'image'|'markdown'|'html'|'text'|'toobig'|'binary', mime? }. mode 'toobig' means
// the content exceeds its type's render cap -> download-only panel; 'binary' means non-image,
// non-text -> download-only.
function classify(path, size, bytes) {
  const ext = extOf(path);
  if (IMAGE_EXT[ext]) {
    return size > IMAGE_RENDER_CAP ? { mode: 'toobig' } : { mode: 'image', mime: IMAGE_EXT[ext] };
  }
  if (looksBinary(bytes)) return { mode: 'binary' };
  // HTML is checked before the shared text cap because it has its own, higher one.
  if (HTML_EXT[ext]) return size > HTML_RENDER_CAP ? { mode: 'toobig' } : { mode: 'html' };
  if (size > TEXT_RENDER_CAP) return { mode: 'toobig' };
  return MD_EXT[ext] ? { mode: 'markdown' } : { mode: 'text' };
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// Minimal, SAFE markdown -> HTML (headings, lists, code fences/spans, links, bold/italic). Every
// piece is escaped first; NO raw-HTML passthrough (untrusted file content, CSP stays strict).
function renderMarkdown(md) {
  const lines = md.split(/\r?\n/);
  const out = [];
  let inCode = false, inUl = false;
  const closeUl = () => { if (inUl) { out.push('</ul>'); inUl = false; } };
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i];
    if (/^```/.test(raw)) {
      if (inCode) { out.push('</code></pre>'); inCode = false; }
      else { closeUl(); out.push('<pre class="mdcode"><code>'); inCode = true; }
      continue;
    }
    if (inCode) { out.push(escapeHtml(raw) + '\n'); continue; }
    const h = /^(#{1,6})\s+(.*)$/.exec(raw);
    if (h) { closeUl(); out.push(`<h${h[1].length}>${inline(h[2])}</h${h[1].length}>`); continue; }
    // GFM table: a header row followed by a |---|:--:|--- separator row, then body rows.
    if (isTableRow(raw) && i + 1 < lines.length && isTableSeparator(lines[i + 1])) {
      closeUl();
      const aligns = parseAligns(lines[i + 1]);
      out.push(`<table class="mdtable"><thead><tr>${
        splitRow(raw).map((cell, j) => `<th${alignAttr(aligns[j])}>${inline(cell)}</th>`).join('')
      }</tr></thead><tbody>`);
      i += 2;   // consumed header + separator
      while (i < lines.length && isTableRow(lines[i])) {
        out.push(`<tr>${splitRow(lines[i]).map((cell, j) => `<td${alignAttr(aligns[j])}>${inline(cell)}</td>`).join('')}</tr>`);
        i++;
      }
      i--;      // for-loop will ++ back to the first non-row line
      out.push('</tbody></table>');
      continue;
    }
    const li = /^\s*[-*]\s+(.*)$/.exec(raw);
    if (li) { if (!inUl) { out.push('<ul>'); inUl = true; } out.push(`<li>${inline(li[1])}</li>`); continue; }
    if (raw.trim() === '') { closeUl(); continue; }
    closeUl();
    out.push(`<p>${inline(raw)}</p>`);
  }
  if (inCode) out.push('</code></pre>');
  closeUl();
  return out.join('\n');

  // A table row has at least one unescaped '|' and isn't a code/heading line.
  function isTableRow(s) { return /\|/.test(s) && s.trim() !== ''; }
  // Separator: cells of only -, :, spaces, with at least one '-' per cell, e.g. |---|:--:|--:|
  function isTableSeparator(s) {
    if (!/\|/.test(s)) return false;
    const cells = splitRow(s);
    return cells.length > 0 && cells.every((c) => /^:?-+:?$/.test(c.trim()));
  }
  // Split "| a | b |" -> ["a","b"], tolerating optional leading/trailing pipes. A backslash-escaped
  // \| is kept as a literal pipe inside a cell (not a delimiter).
  function splitRow(s) {
    let t = s.trim().replace(/^\|/, '').replace(/\|$/, '');
    const cells = [];
    let cur = '';
    for (let k = 0; k < t.length; k++) {
      if (t[k] === '\\' && t[k + 1] === '|') { cur += '|'; k++; }
      else if (t[k] === '|') { cells.push(cur.trim()); cur = ''; }
      else cur += t[k];
    }
    cells.push(cur.trim());
    return cells;
  }
  function parseAligns(sep) {
    return splitRow(sep).map((c) => {
      const t = c.trim();
      const l = t.startsWith(':'), r = t.endsWith(':');
      return r && l ? 'center' : r ? 'right' : l ? 'left' : '';
    });
  }
  function alignAttr(a) { return a ? ` style="text-align:${a}"` : ''; }

  // inline spans: escape THEN apply a small set of patterns on the escaped text.
  function inline(s) {
    let t = escapeHtml(s);
    t = t.replace(/`([^`]+)`/g, (_m, c) => `<code>${c}</code>`);
    t = t.replace(/\*\*([^*]+)\*\*/g, (_m, c) => `<strong>${c}</strong>`);
    t = t.replace(/(^|[^*])\*([^*]+)\*/g, (_m, p, c) => `${p}<em>${c}</em>`);
    // links [text](url) — only http/https/ftp/mailto, opened via the app's openExternal
    t = t.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+|ftp:\/\/[^\s)]+|mailto:[^\s)]+)\)/g,
      (_m, txt, url) => `<a href="#" data-url="${escapeHtml(url)}" class="mdlink">${txt}</a>`);
    return t;
  }
}

function createFileViewerTab(spec, ctx) {
  const { id: sessionId, path, api } = spec;
  let el = null;
  let mounted = false;
  let fetched = null;   // { data_b64, size, truncated } once loaded
  // Scripted-HTML opt-in (§16), per TAB and per SESSION — never persisted, so reopening a file
  // starts static again and the choice can't silently carry over to a different file.
  let scripted = false;
  let scriptedUrl = null;

  function h(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    if (attrs) for (const k in attrs) {
      if (k === 'style') e.style.cssText = attrs[k];
      else if (k === 'class') e.className = attrs[k];
      else e.setAttribute(k, attrs[k]);
    }
    for (const kid of kids) e.appendChild(typeof kid === 'string' ? document.createTextNode(kid) : kid);
    return e;
  }

  function toolbar(size, note, mode) {
    const bar = h('div', { class: 'fv-bar' });
    bar.appendChild(h('span', { class: 'fv-name' }, baseName(path)));
    const meta = h('span', { class: 'fv-meta' }, fmtSize(size) + (note ? ' · ' + note : ''));
    bar.appendChild(meta);
    // "Enable scripts" — opt THIS document into a scripted preview. Only offered for html, and only
    // while still static: running a remote file's JS is a per-file decision, so there is no
    // remembered preference and no auto-enable. See the fv-html branch for the isolation.
    if (mode === 'html' && !scripted && api.enableHtmlScripts) {
      const en = h('button', { class: 'fv-scripts', title:
        'Run this file\'s scripts in an isolated frame (it will be able to load code from the network)' },
        'Enable scripts');
      en.onclick = async () => {
        if (!fetched) return;
        en.disabled = true; en.textContent = 'Enabling…';
        try {
          const res = await api.enableHtmlScripts(fetched.data_b64);
          if (!res || !res.url) throw new Error('no preview url');
          scriptedUrl = res.url;
          scripted = true;
          ctx.setStatus('scripts enabled for ' + baseName(path));
          if (el) renderInto(el);   // re-render into the scripted frame
        } catch (e) {
          ctx.setStatus('enable scripts failed: ' + (e && e.message || e));
          en.disabled = false; en.textContent = 'Enable scripts';
        }
      };
      bar.appendChild(en);
    }
    const dl = h('button', { class: 'fv-dl' }, 'Download to local');
    dl.onclick = async () => {
      if (!fetched) return;
      dl.disabled = true; dl.textContent = 'Saving…';
      try {
        const res = await api.saveFile(fetched.data_b64, baseName(path));
        ctx.setStatus(res && res.ok ? `saved ${baseName(path)}` : 'save canceled');
      } catch (e) { ctx.setStatus('save failed: ' + (e && e.message || e)); }
      dl.disabled = false; dl.textContent = 'Download to local';
    };
    bar.appendChild(dl);
    return bar;
  }

  function fmtSize(n) {
    if (n < 1024) return n + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
    return (n / 1024 / 1024).toFixed(1) + ' MB';
  }

  function renderInto(container) {
    container.innerHTML = '';
    const body = h('div', { class: 'fv-body' });
    if (!fetched) { body.appendChild(h('div', { class: 'fv-msg' }, 'Loading…')); container.appendChild(body); return; }

    const bytes = b64ToBytes(fetched.data_b64);
    const c = classify(path, fetched.size, bytes);
    // Classify BEFORE building the toolbar: the mode contributes a note and the Enable-scripts button.
    const notes = [];
    if (fetched.truncated) notes.push('truncated');
    // Say which of the two html modes is in effect, so an inert page reads as intended rather than
    // as a bug, and a scripted one is never silent about it.
    if (c.mode === 'html') notes.push(scripted ? 'scripts ENABLED' : 'scripts disabled');
    container.appendChild(toolbar(fetched.size, notes.join(' · '), c.mode));

    if (c.mode === 'image') {
      const img = h('img', { class: 'fv-img' });
      img.src = `data:${c.mime};base64,${fetched.data_b64}`;
      body.appendChild(img);
    } else if (c.mode === 'toobig') {
      body.appendChild(h('div', { class: 'fv-msg' }, `File is ${fmtSize(fetched.size)} — too large to preview. Use Download.`));
    } else if (c.mode === 'binary') {
      body.appendChild(h('div', { class: 'fv-msg' }, `Binary file (${fmtSize(fetched.size)}) — no preview. Use Download.`));
    } else if (c.mode === 'html') {
      // HTML preview. Unlike markdown (which we transpile to escaped HTML ourselves), here the
      // file's own markup IS the render, so it goes to the browser parser. Two modes:
      //
      // STATIC (default): sandbox="" + srcdoc. Renders the file's own CSS and embedded data:
      // images; runs nothing. Two INDEPENDENT layers, each measured sufficient alone in a real
      // WKWebView:
      //   1. sandbox="" — no allow-scripts (no JS), no allow-same-origin (opaque origin: no access
      //      to our DOM/localStorage, no reach to window.__TAURI__ / the invoke bridge), no
      //      allow-popups, no allow-top-navigation, no allow-forms.
      //   2. The app CSP (script-src 'self') is INHERITED by the srcdoc document, so inline
      //      <script> and inline handlers (onerror=/onload=) are blocked even if the sandbox
      //      attribute were loosened later by mistake.
      // srcdoc rather than a blob: URL because blob: is a separate origin that default-src 'self'
      // refuses to load as a frame — measured: the frame stays blank.
      //
      // SCRIPTED (after the user clicks "Enable scripts" for this one file): the document is served
      // from the buoyhtml: scheme — a SEPARATE ORIGIN with its own per-response CSP that permits
      // inline script and https: subresources. This is why it's a custom protocol and not just a
      // looser attribute: a srcdoc child can only ever INTERSECT the parent CSP, so making srcdoc
      // scriptable would require 'unsafe-inline' on the APP's script-src — and the app renders
      // untrusted terminal output, so that would trade a contained problem for app-origin XSS.
      // The frame gets allow-scripts but still NOT allow-same-origin, so the origin stays opaque
      // and wry's main-frame-only IPC injection never reaches it: __TAURI__, __TAURI_INTERNALS__
      // and window.ipc are all undefined there, parent/top access throws, and a direct invoke()
      // attempt fails — all measured against a hostile page. See src-tauri/src/html_preview.rs.
      const frame = h('iframe', {
        class: 'fv-html',
        // Scripts, and nothing else: still no allow-same-origin (opaque origin), no
        // allow-popups, no allow-top-navigation, no allow-forms, no allow-modals.
        sandbox: scripted ? 'allow-scripts' : '',
        referrerpolicy: 'no-referrer',
        // Empty allow-list: no camera/mic/geolocation/etc even in scripted mode.
        allow: '',
      });
      if (scripted) frame.src = scriptedUrl;
      else frame.srcdoc = new TextDecoder('utf-8').decode(bytes);
      body.appendChild(frame);
    } else if (c.mode === 'markdown') {
      const md = h('div', { class: 'fv-md' });
      md.innerHTML = renderMarkdown(new TextDecoder('utf-8').decode(bytes));  // renderMarkdown escapes all content
      md.querySelectorAll('a.mdlink').forEach((a) => {
        a.onclick = (e) => { e.preventDefault(); api.openExternal(a.getAttribute('data-url')); };
      });
      body.appendChild(md);
    } else {
      const pre = h('pre', { class: 'fv-text' });
      pre.textContent = new TextDecoder('utf-8').decode(bytes);   // NEVER innerHTML for untrusted content
      body.appendChild(pre);
    }
    container.appendChild(body);
  }

  return {
    kind: 'fileviewer',
    get mounted() { return mounted; },

    async mount(container) {
      el = h('div', { class: 'fv-root', style: 'width:100%;height:100%;overflow:auto;' });
      container.appendChild(el);
      mounted = true;
      renderInto(el);   // shows "Loading…"
      if (!fetched) {
        try {
          fetched = await api.readRemoteFile(sessionId, path);
        } catch (e) {
          el.innerHTML = '';
          el.appendChild(h('div', { class: 'fv-msg fv-err' }, 'Could not open: ' + (e && e.message || e)));
          ctx.setStatus('open failed: ' + baseName(path));
          return;
        }
        if (mounted) renderInto(el);
      }
    },
    element() { return el; },
    onData() { /* viewers ignore terminal data */ },
    fit() { return null; },   // no grid; nothing to report to the backend
    resize() {},
    focus() { if (el) el.focus && el.focus(); },
    readBuffer() { return el ? el.textContent : ''; },   // test hook
    dispose() { if (el && el.parentNode) el.parentNode.removeChild(el); el = null; mounted = false; },
  };
}

if (typeof module !== 'undefined' && module.exports) module.exports = { createFileViewerTab, renderMarkdown, extOf, classify, TEXT_RENDER_CAP, IMAGE_RENDER_CAP, HTML_RENDER_CAP };
if (typeof window !== 'undefined') window.DTFileViewerTab = { createFileViewerTab, renderMarkdown, extOf, classify };
