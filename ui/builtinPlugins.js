'use strict';
// Built-in link plugins: URL and filesystem path. They use the SAME registry API a
// third-party plugin would (see src/shared/plugins.js), so they are examples as much as
// features. Register them, then register your own for tickets/PRs/etc.
/* global module */

// URL: http(s)/ftp/file, and bare www. — trailing punctuation trimmed by the regex boundary.
const URL_RE = /\b(?:https?|ftp|file):\/\/[^\s"'<>`)]+|(?:\bwww\.[^\s"'<>`)]+)/g;

// Path: absolute (/a/b), home (~/a/b), or relative with a slash (./x, ../x, src/a). Avoids
// matching lone words; requires at least one slash so it doesn't flag every bare token.
const PATH_RE = /(?:~\/|\.{1,2}\/|\/)[^\s"'<>`:]+/g;

// Build the two default plugins. Handlers are thin — they call into ctx, which the host
// (renderer) supplies with openExternal / copyText / setStatus / meta.
function builtinLinkPlugins() {
  return [
    {
      name: 'url',
      priority: 10,   // URLs win over paths when they'd overlap (e.g. file:///a/b)
      regex: URL_RE,
      onClick(text, ctx) {
        let url = text;
        if (/^www\./i.test(url)) url = 'https://' + url;
        // Only open safe schemes (terminal text is untrusted).
        if (!/^(https?|ftp|file):\/\//i.test(url)) { ctx.setStatus('refused to open: ' + text); return; }
        ctx.openExternal(url);
        ctx.setStatus('opened ' + url);
      },
    },
    {
      name: 'path',
      priority: 0,
      regex: PATH_RE,
      onClick(text, ctx) {
        // A path in the terminal is usually REMOTE (session is ssh+tmux), so the app can't
        // open it locally in general — hence a callback. Default: copy + inform. A plugin
        // can override this behavior by registering a higher-priority path matcher.
        ctx.copyText(text);
        const where = ctx.meta && ctx.meta.host ? `remote (${ctx.meta.host})` : 'local';
        ctx.setStatus(`copied ${where} path: ${text}`);
      },
    },
  ];
}

// UMD-lite: CommonJS for tests, global for the sandboxed renderer (<script> tag).
if (typeof module !== 'undefined' && module.exports) module.exports = { builtinLinkPlugins, URL_RE, PATH_RE };
if (typeof window !== 'undefined') window.DTBuiltinPlugins = { builtinLinkPlugins, URL_RE, PATH_RE };
