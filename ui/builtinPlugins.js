'use strict';
// Built-in link plugins: URL and filesystem path. They use the SAME registry API a
// third-party plugin would (see src/shared/plugins.js), so they are examples as much as
// features. Register them, then register your own for tickets/PRs/etc.
/* global module */

// URL: http(s)/ftp/file, bare www., AND bare loopback host:port (localhost:3000, 127.0.0.1:8080
// — with optional scheme + path). The bare-loopback form makes dev-server output clickable so it
// can be port-forwarded (§18). Trailing punctuation trimmed by the char classes.
const URL_RE = /\b(?:https?|ftp|file):\/\/[^\s"'<>`)]+|(?:\bwww\.[^\s"'<>`)]+)|(?:\b(?:localhost|127\.0\.0\.1):\d{1,5}(?:\/[^\s"'<>`)]*)?)/g;

// Path matcher. Four kinds, tried left-to-right in one alternation:
//   1. slash paths: absolute (/a/b), home (~/a/b), or slash-containing relative (./x, ../x, src/a)
//   2. bare filename WITH an extension: README.md, notes.txt, pic.png (>=1 non-space, a dot, then
//      1-8 ext chars) — this makes `ls` output clickable (§17). Relatives are resolved against the
//      pane cwd in the backend.
//   3. known extension-less names: Makefile, Dockerfile, LICENSE, README (word-bounded).
// A plain word with no slash, no extension, and not on the known list stays unmatched (avoids
// underlining every token). Trailing shell punctuation is excluded from the char classes.
const KNOWN_NAMES = ['Makefile', 'Dockerfile', 'LICENSE', 'README', 'Gemfile', 'Rakefile'];
// 1. leading-anchored slash paths: absolute, home, or ./ ../ relative. The char class excludes
//    ')' (and '(') so a path wrapped in a tool call — e.g. Update(/a/b/x.md) — doesn't swallow the
//    trailing paren (matches URL_RE's exclusions). Paths literally containing parens are rare and
//    not worth mis-capturing every wrapped path for.
const SLASH_PATH = String.raw`(?:~\/|\.{1,2}\/|\/)[^\s"'<>\`:()]+`;
// 2. relative path with an interior slash and a filename WITH an extension: src/main.rs, a/b/c.txt.
//    (requires an extension on the last segment so bare dir words like "src/foo" without a dot
//    still match here only when the final segment has an ext — keeps noise down.)
const REL_SLASH = String.raw`[\w.\-]+(?:\/[\w.\-]+)*\/[\w.\-]*\w\.[A-Za-z0-9]{1,8}`;
// 3. bare filename WITH an extension: README.md, notes.txt.
const BARE_WITH_EXT = String.raw`[\w.\-]*\w\.[A-Za-z0-9]{1,8}`;
// 4. known extension-less names.
const KNOWN_RE = String.raw`\b(?:${KNOWN_NAMES.join('|')})\b`;
const PATH_RE = new RegExp(`${SLASH_PATH}|${REL_SLASH}|${BARE_WITH_EXT}|${KNOWN_RE}`, 'g');

// Build the two default plugins. Handlers are thin — they call into ctx, which the host
// (renderer) supplies with openExternal / copyText / setStatus / meta.
// Smart URL open, shared by the regex 'url' link plugin AND the OSC 8 hyperlink handler (§21) so
// both behave identically. mods = { shift, meta, alt } (§18): plain = smart (loopback -> ssh -L
// tunnel + local browser; else default browser); Shift(+Cmd) = chooser (where to open). Terminal
// text is untrusted, so only safe schemes are opened.
function openUrlSmart(text, ctx, mods) {
  if (mods && mods.shift && ctx.chooseOpen) { ctx.chooseOpen(text); return; }
  if (ctx.isLoopback && ctx.isLoopback(text) && ctx.openForwardedUrl) { ctx.openForwardedUrl(text); return; }
  let url = text;
  if (/^www\./i.test(url)) url = 'https://' + url;
  if (!/^(https?|ftp|file):\/\//i.test(url)) { ctx.setStatus('refused to open: ' + text); return; }
  ctx.openExternal(url);
  ctx.setStatus('opened ' + url);
}

// §21: parse a file:// URI into an absolute remote path, or null if it isn't a file URI. Handles
// `file:///abs`, `file://host/abs` (host ignored — CC uses an empty host), percent-encoding, and a
// trailing `:line[:col]` suffix some tools append (stripped so the path resolves). Returns null for
// anything that isn't file://, so callers fall through to URL handling.
function parseFileUri(uri) {
  const m = /^file:\/\/[^/]*(\/[^\s]*)$/i.exec(String(uri).trim());
  if (!m) return null;
  let path = m[1];
  try { path = decodeURIComponent(path); } catch (_) { /* keep raw on bad encoding */ }
  // strip a trailing :line or :line:col (but NOT a lone drive-like ":" mid-path)
  path = path.replace(/:\d+(?::\d+)?$/, '');
  return path;
}

// §21: extract OSC 8 file:// hyperlinks from a raw output chunk as [{shown, path}] pairs (path is
// the absolute path from the file:// URI). Format: ESC ] 8 ; params ; uri (ST|BEL) text ESC ] 8 ; ; (ST|BEL).
// Non-file links and malformed sequences are skipped. Pure/testable; the renderer builds a lookup
// map from these so a clicked relative filename resolves to the agent's authoritative absolute path.
const OSC8_LINK_RE = /\x1b\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\)([^\x1b]*)\x1b\]8;;(?:\x07|\x1b\\)/g;
function extractOsc8FileLinks(data) {
  const out = [];
  if (!data || data.indexOf('\x1b]8;') === -1) return out;
  OSC8_LINK_RE.lastIndex = 0;
  let m;
  while ((m = OSC8_LINK_RE.exec(data)) !== null) {
    const shown = (m[2] || '').trim();
    const path = parseFileUri(m[1]);
    if (shown && path) out.push({ shown, path });
  }
  return out;
}

function builtinLinkPlugins() {
  return [
    {
      name: 'url',
      priority: 10,   // URLs win over paths when they'd overlap (e.g. file:///a/b)
      regex: URL_RE,
      onClick(text, ctx, mods) { openUrlSmart(text, ctx, mods); },
    },
    {
      name: 'path',
      priority: 0,
      regex: PATH_RE,
      onClick(text, ctx) {
        // Open the path in an in-app file-viewer tab (§16): the host fetches the file's bytes
        // (remote over ssh, or local) and previews text/markdown/image with a Download button.
        // Falls back to copy+status if the host can't provide a viewer (older/plain builds).
        if (ctx.openViewer) { ctx.openViewer(text); return; }
        ctx.copyText(text);
        const where = ctx.meta && ctx.meta.host ? `remote (${ctx.meta.host})` : 'local';
        ctx.setStatus(`copied ${where} path: ${text}`);
      },
    },
  ];
}

// UMD-lite: CommonJS for tests, global for the sandboxed renderer (<script> tag).
if (typeof module !== 'undefined' && module.exports) module.exports = { builtinLinkPlugins, openUrlSmart, parseFileUri, extractOsc8FileLinks, URL_RE, PATH_RE, KNOWN_NAMES };
if (typeof window !== 'undefined') window.DTBuiltinPlugins = { builtinLinkPlugins, openUrlSmart, parseFileUri, extractOsc8FileLinks, URL_RE, PATH_RE, KNOWN_NAMES };
