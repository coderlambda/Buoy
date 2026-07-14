'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { PluginRegistry } = require('../src/shared/plugins');
// The Tauri app serves ui/ ; that's the live copy of the link plugins.
const { builtinLinkPlugins, URL_RE, PATH_RE } = require('../ui/builtinPlugins');

function reg() {
  const r = new PluginRegistry();
  builtinLinkPlugins().forEach((p) => r.registerLink(p));
  return r;
}

// TC-PL1 URL detection
test('TC-PL1 detects urls', () => {
  const r = reg();
  const m = r.findMatches('see https://example.com/x and http://a.b/c');
  const urls = m.filter((x) => x.plugin.name === 'url').map((x) => x.text);
  assert.deepEqual(urls, ['https://example.com/x', 'http://a.b/c']);
});

// TC-PL2 path detection (absolute, home, relative-with-slash)
test('TC-PL2 detects paths', () => {
  const r = reg();
  const m = r.findMatches('open /etc/hosts or ~/notes/todo.md or ./src/a.js');
  const paths = m.filter((x) => x.plugin.name === 'path').map((x) => x.text);
  assert.deepEqual(paths, ['/etc/hosts', '~/notes/todo.md', './src/a.js']);
});

// TC-PL3 plain words (no slash, no extension, not a known name) are NOT paths
test('TC-PL3 plain words are not paths', () => {
  const r = reg();
  const m = r.findMatches('just some words here and the src dir');
  assert.equal(m.length, 0);
});

// TC-PL3b bare filenames WITH an extension ARE paths (§17 — makes `ls` output clickable)
test('TC-PL3b bare filenames with extensions are paths', () => {
  const r = reg();
  const m = r.findMatches('README.md  notes.txt  pic.png');
  const paths = m.filter((x) => x.plugin.name === 'path').map((x) => x.text);
  assert.deepEqual(paths, ['README.md', 'notes.txt', 'pic.png']);
});

// TC-PL3c relative slash paths and known extension-less names
test('TC-PL3c relative slash paths and known names', () => {
  const r = reg();
  const m = r.findMatches('build src/main.rs then Makefile and Dockerfile');
  const paths = m.filter((x) => x.plugin.name === 'path').map((x) => x.text);
  assert.deepEqual(paths, ['src/main.rs', 'Makefile', 'Dockerfile']);
});

// TC-PL4 priority: url wins over path on overlap (file:///a/b)
test('TC-PL4 url wins over path on overlap', () => {
  const r = reg();
  const m = r.findMatches('file:///var/log/x');
  assert.equal(m.length, 1);
  assert.equal(m[0].plugin.name, 'url');
  assert.equal(m[0].text, 'file:///var/log/x');
});

// TC-PL4b loopback URLs are detected (localhost/127.0.0.1 with a port), §18
test('TC-PL4b detects loopback urls', () => {
  const r = reg();
  const m = r.findMatches('vite at http://localhost:5173/ and 127.0.0.1:8080 up');
  const urls = m.filter((x) => x.plugin.name === 'url').map((x) => x.text);
  assert.deepEqual(urls, ['http://localhost:5173/', '127.0.0.1:8080']);
});

// TC-PL4c plain click on a loopback URL routes to openForwardedUrl (tunnel), not openExternal
test('TC-PL4c loopback click forwards; plain url opens external', () => {
  const url = builtinLinkPlugins().find((p) => p.name === 'url');
  let forwarded = null, external = null;
  const ctx = {
    isLoopback: (u) => /^(?:https?:\/\/)?(localhost|127\.0\.0\.1):\d+/.test(u),
    openForwardedUrl: (u) => { forwarded = u; },
    openExternal: (u) => { external = u; },
    setStatus() {},
  };
  url.onClick('http://localhost:3000/app', ctx, { shift: false, meta: true });
  assert.equal(forwarded, 'http://localhost:3000/app', 'loopback -> forwarded');
  assert.equal(external, null, 'loopback not opened externally');

  forwarded = null;
  url.onClick('https://github.com/x', ctx, { shift: false, meta: true });
  assert.equal(external, 'https://github.com/x', 'plain -> external');
  assert.equal(forwarded, null);
});

// TC-PL4d Shift+Cmd click routes to the chooser
test('TC-PL4d shift-click opens the chooser', () => {
  const url = builtinLinkPlugins().find((p) => p.name === 'url');
  let chosen = null, forwarded = null;
  const ctx = {
    isLoopback: () => true, openForwardedUrl: (u) => { forwarded = u; },
    chooseOpen: (u) => { chosen = u; }, openExternal() {}, setStatus() {},
  };
  url.onClick('localhost:3000', ctx, { shift: true, meta: true });
  assert.equal(chosen, 'localhost:3000', 'shift -> chooser');
  assert.equal(forwarded, null, 'shift does not auto-forward');
});

// TC-PL5 custom plugin registers and matches; unregister works
test('TC-PL5 custom plugin + unregister', () => {
  const r = new PluginRegistry();
  let clicked = null;
  const un = r.registerLink({ name: 'jira', regex: /\b[A-Z]+-\d+\b/g, onClick: (t) => { clicked = t; } });
  let m = r.findMatches('fix ABC-123 now');
  assert.equal(m.length, 1);
  assert.equal(m[0].text, 'ABC-123');
  m[0].plugin.onClick(m[0].text, {});
  assert.equal(clicked, 'ABC-123');
  un();
  assert.equal(r.findMatches('fix ABC-123 now').length, 0);
});

// TC-PL6 non-global regex is rejected
test('TC-PL6 rejects non-global regex', () => {
  const r = new PluginRegistry();
  assert.throws(() => r.registerLink({ name: 'x', regex: /foo/, onClick() {} }), /global/);
});

// TC-PL7 higher-priority custom plugin claims a range before a built-in
test('TC-PL7 priority overlap resolution', () => {
  const r = reg();
  // a custom high-priority matcher for a specific path
  r.registerLink({ name: 'special', priority: 100, regex: /\/etc\/hosts/g, onClick() {} });
  const m = r.findMatches('/etc/hosts');
  assert.equal(m.length, 1);
  assert.equal(m[0].plugin.name, 'special');
});

// TC-PL8 matches returned sorted by start, non-overlapping
test('TC-PL8 sorted non-overlapping', () => {
  const r = reg();
  const m = r.findMatches('a /x/y b https://z.co c ~/d');
  const starts = m.map((x) => x.start);
  assert.deepEqual(starts, [...starts].sort((a, b) => a - b));
  for (let i = 1; i < m.length; i++) assert.ok(m[i].start >= m[i - 1].end, 'no overlap');
});

// TC-PL9 tab-kind registry: register/create/has/unregister (polymorphic tabs, §14/§15)
test('TC-PL9 tab-kind registry', () => {
  const r = new PluginRegistry();
  assert.equal(r.hasTabKind('terminal'), false);
  let created = null;
  const un = r.registerTabKind({ kind: 'markdown', create: (spec) => { created = spec; return { kind: 'markdown', mount(){}, dispose(){} }; } });
  assert.equal(r.hasTabKind('markdown'), true);
  const content = r.createTabContent('markdown', { file: 'a.md' }, {});
  assert.equal(content.kind, 'markdown');
  assert.deepEqual(created, { file: 'a.md' });
  un();
  assert.equal(r.hasTabKind('markdown'), false);
  assert.throws(() => r.createTabContent('markdown', {}, {}), /no tab-kind/);
});

// TC-PL10 tab-kind provider validation
test('TC-PL10 rejects bad tab-kind provider', () => {
  const r = new PluginRegistry();
  assert.throws(() => r.registerTabKind({ kind: 'x' }), /create/);
  assert.throws(() => r.registerTabKind({ create() {} }), /kind/);
});
