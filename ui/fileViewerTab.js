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

const IMAGE_EXT = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif',
  webp: 'image/webp', bmp: 'image/bmp', svg: 'image/svg+xml', ico: 'image/x-icon' };
const MD_EXT = { md: 1, markdown: 1, mdown: 1, mkd: 1 };

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
// Returns { mode: 'image'|'markdown'|'text'|'toobig'|'binary', mime? }. mode 'toobig' means the
// content exceeds its type's render cap -> download-only panel; 'binary' means non-image,
// non-text -> download-only.
function classify(path, size, bytes) {
  const ext = extOf(path);
  if (IMAGE_EXT[ext]) {
    return size > IMAGE_RENDER_CAP ? { mode: 'toobig' } : { mode: 'image', mime: IMAGE_EXT[ext] };
  }
  if (looksBinary(bytes)) return { mode: 'binary' };
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
  for (const raw of lines) {
    if (/^```/.test(raw)) {
      if (inCode) { out.push('</code></pre>'); inCode = false; }
      else { closeUl(); out.push('<pre class="mdcode"><code>'); inCode = true; }
      continue;
    }
    if (inCode) { out.push(escapeHtml(raw) + '\n'); continue; }
    const h = /^(#{1,6})\s+(.*)$/.exec(raw);
    if (h) { closeUl(); out.push(`<h${h[1].length}>${inline(h[2])}</h${h[1].length}>`); continue; }
    const li = /^\s*[-*]\s+(.*)$/.exec(raw);
    if (li) { if (!inUl) { out.push('<ul>'); inUl = true; } out.push(`<li>${inline(li[1])}</li>`); continue; }
    if (raw.trim() === '') { closeUl(); continue; }
    closeUl();
    out.push(`<p>${inline(raw)}</p>`);
  }
  if (inCode) out.push('</code></pre>');
  closeUl();
  return out.join('\n');

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

  function toolbar(size, note) {
    const bar = h('div', { class: 'fv-bar' });
    bar.appendChild(h('span', { class: 'fv-name' }, baseName(path)));
    const meta = h('span', { class: 'fv-meta' }, fmtSize(size) + (note ? ' · ' + note : ''));
    bar.appendChild(meta);
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
    const truncNote = fetched.truncated ? 'truncated' : '';
    container.appendChild(toolbar(fetched.size, truncNote));

    const c = classify(path, fetched.size, bytes);
    if (c.mode === 'image') {
      const img = h('img', { class: 'fv-img' });
      img.src = `data:${c.mime};base64,${fetched.data_b64}`;
      body.appendChild(img);
    } else if (c.mode === 'toobig') {
      body.appendChild(h('div', { class: 'fv-msg' }, `File is ${fmtSize(fetched.size)} — too large to preview. Use Download.`));
    } else if (c.mode === 'binary') {
      body.appendChild(h('div', { class: 'fv-msg' }, `Binary file (${fmtSize(fetched.size)}) — no preview. Use Download.`));
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

if (typeof module !== 'undefined' && module.exports) module.exports = { createFileViewerTab, renderMarkdown, extOf, classify, TEXT_RENDER_CAP, IMAGE_RENDER_CAP };
if (typeof window !== 'undefined') window.DTFileViewerTab = { createFileViewerTab, renderMarkdown, extOf, classify };
