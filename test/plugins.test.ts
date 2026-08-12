
import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { PluginRegistry } from '../ui/src/plugins.js';
// The Tauri app serves ui/ ; that's the live copy of the link plugins.
import {
  builtinLinkPlugins,
  containsTuiRepaint,
  createOscNotificationParser,
  createTuiActivityTracker,
  extractOsc8FileLinks,
  isOscNotification,
  openUrlSmart,
  parseFileUri,
  stripOsc8Sequences,
} from '../ui/src/builtinPlugins.js';

function reg() {
  const r = new PluginRegistry();
  builtinLinkPlugins().forEach((p) => r.registerLink(p));
  return r;
}

function fixturePath(name: string): string {
  return join(__dirname, 'fixtures', name);
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

// TC-PL2b a path wrapped in a tool call — Update(/a/b/x.md) — must NOT swallow the trailing ')'
test('TC-PL2b paren-wrapped path excludes the trailing paren', () => {
  const r = reg();
  const m = r.findMatches('Update(/local/home/y/w/proj/docs/data-migration-plan.md)');
  const paths = m.filter((x) => x.plugin.name === 'path').map((x) => x.text);
  assert.deepEqual(paths, ['/local/home/y/w/proj/docs/data-migration-plan.md'], 'no trailing )');
  // and a paren-wrapped relative dir path stays intact
  const m2 = r.findMatches('Bash(cd /srv/app/bin)');
  assert.deepEqual(m2.filter((x) => x.plugin.name === 'path').map((x) => x.text), ['/srv/app/bin']);
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
  const match = m[0];
  assert.ok(match);
  assert.equal(match.plugin.name, 'url');
  assert.equal(match.text, 'file:///var/log/x');
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
  let forwarded: string | null = null, external: string | null = null;
  const ctx = {
    isLoopback: (u: string) => /^(?:https?:\/\/)?(localhost|127\.0\.0\.1):\d+/.test(u),
    openForwardedUrl: (u: string) => { forwarded = u; },
    openExternal: (u: string) => { external = u; },
    setStatus() {},
  };
  assert.ok(url);
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
  let chosen: string | null = null, forwarded: string | null = null;
  const ctx = {
    isLoopback: () => true, openForwardedUrl: (u: string) => { forwarded = u; },
    chooseOpen: (u: string) => { chosen = u; }, openExternal() {}, setStatus() {},
  };
  assert.ok(url);
  url.onClick('localhost:3000', ctx, { shift: true, meta: true });
  assert.equal(chosen, 'localhost:3000', 'shift -> chooser');
  assert.equal(forwarded, null, 'shift does not auto-forward');
});

// TC-PL4e §21: the OSC 8 hyperlink handler uses the SAME openUrlSmart as the regex 'url' plugin,
// so an embedded-URI link routes identically (loopback -> tunnel; else scheme-checked external;
// unsafe scheme refused).
test('TC-PL4e OSC 8 handler routes via openUrlSmart', () => {
  let forwarded: string | null = null, external: string | null = null, status: string | null = null;
  const ctx = {
    isLoopback: (u: string) => /^(?:https?:\/\/)?(localhost|127\.0\.0\.1):\d+/.test(u),
    openForwardedUrl: (u: string) => { forwarded = u; },
    openExternal: (u: string) => { external = u; },
    setStatus: (m: string) => { status = m; },
  };
  openUrlSmart('http://localhost:8080/', ctx, {});
  assert.equal(forwarded, 'http://localhost:8080/', 'loopback OSC8 -> tunnel');
  openUrlSmart('https://example.com', ctx, {});
  assert.equal(external, 'https://example.com', 'plain OSC8 -> external');
  external = null;
  openUrlSmart('javascript:alert(1)', ctx, {});   // unsafe scheme
  assert.equal(external, null, 'unsafe scheme not opened');
  assert.match(status ?? '', /refused/, 'unsafe scheme refused with status');
});

// TC-PL4f §21: parseFileUri extracts the absolute remote path from an OSC 8 file:// URI (Claude
// Code emits these for file tool-calls), returns null for non-file URIs so callers fall through.
test('TC-PL4f parseFileUri', () => {
  // classic file:/// with empty host (what Claude Code emits)
  assert.equal(parseFileUri('file:///local/home/y/w/index.ts'), '/local/home/y/w/index.ts');
  // file://host/path — host ignored, path kept absolute
  assert.equal(parseFileUri('file://somehost/etc/hosts'), '/etc/hosts');
  // percent-encoding decoded (spaces etc.)
  assert.equal(parseFileUri('file:///a/My%20Docs/x.md'), '/a/My Docs/x.md');
  // trailing :line and :line:col stripped so the path resolves
  assert.equal(parseFileUri('file:///a/foo.rs:42'), '/a/foo.rs');
  assert.equal(parseFileUri('file:///a/foo.rs:42:7'), '/a/foo.rs');
  // non-file URIs -> null (handler falls through to openUrlSmart)
  assert.equal(parseFileUri('https://example.com'), null);
  assert.equal(parseFileUri('localhost:3000'), null);
  assert.equal(parseFileUri('mailto:x@y.com'), null);
});

// TC-PL4g §21: extractOsc8FileLinks harvests display-text -> absolute path from raw OSC 8 output
// (the renderer maps these so a clicked relative filename opens the agent's authoritative abs path).
test('TC-PL4g extractOsc8FileLinks', () => {
  const E = '\x1b';
  // real Claude Code shape: id= param, file:// abs uri (ST terminator), short display text
  const st = E + ']8;id=lakfgb;file:///local/home/y/w/proj/README.md' + E + '\\' + 'README.md' + E + ']8;;' + E + '\\';
  assert.deepEqual(extractOsc8FileLinks('pre ' + st + ' post'),
    [{ shown: 'README.md', path: '/local/home/y/w/proj/README.md' }]);
  // BEL-terminated variant
  const bel = E + ']8;;file:///a/b/x.ts\x07' + 'src/x.ts' + E + ']8;;\x07';
  assert.deepEqual(extractOsc8FileLinks(bel), [{ shown: 'src/x.ts', path: '/a/b/x.ts' }]);
  // multiple links in one chunk
  const two = st + 'noise' + bel;
  assert.equal(extractOsc8FileLinks(two).length, 2);
  // non-file OSC 8 (e.g. http hyperlink) is ignored (path null)
  const http = E + ']8;;https://example.com' + E + '\\' + 'example' + E + ']8;;' + E + '\\';
  assert.deepEqual(extractOsc8FileLinks(http), []);
  // no OSC 8 at all -> empty
  assert.deepEqual(extractOsc8FileLinks('just plain text README.md'), []);
});

// TC-PL4g2: capture-pane snapshots retain OSC 8 wrappers, which xterm renders as persistent
// dotted/dashed link underlines. Removing just those wrappers keeps the text and all SGR styling.
test('TC-PL4g2 stripOsc8Sequences removes hyperlink wrappers only', () => {
  const E = '\x1b';
  const st = E + ']8;id=one;file:///a/README.md' + E + '\\README.md' + E + ']8;;' + E + '\\';
  const bel = E + ']8;;https://example.com\x07example' + E + ']8;;\x07';
  const styled = E + '[31m' + st + E + '[0m ' + E + '[4:4mkept dotted style' + E + '[0m';

  assert.equal(stripOsc8Sequences(styled),
    E + '[31mREADME.md' + E + '[0m ' + E + '[4:4mkept dotted style' + E + '[0m');
  assert.equal(stripOsc8Sequences(bel), 'example');
  assert.equal(stripOsc8Sequences('plain text'), 'plain text');
  assert.equal(stripOsc8Sequences(E + ']2;window title\x07text'), E + ']2;window title\x07text',
    'unrelated OSC protocols are preserved');
});

// TC-PL4h notification OSC detection covers the protocols advertised by modern agent terminals.
test('TC-PL4h recognizes OSC 9/777/99 notification payloads only', () => {
  assert.equal(isOscNotification('9;Agent needs input'), true);
  assert.equal(isOscNotification('777;notify;Codex;Task complete'), true);
  assert.equal(isOscNotification('777;other;Codex;Task complete'), false);
  assert.equal(isOscNotification('99;;Simple notification'), true);
  assert.equal(isOscNotification('99;i=one:d=0;p=title;Build'), false, 'unfinished Kitty chunk');
  assert.equal(isOscNotification('99;i=one:p=body;Done'), true, 'final Kitty body chunk');
  assert.equal(isOscNotification('99;i=one:p=close;'), false, 'close report/control');
  assert.equal(isOscNotification('99;i=one:p=alive;'), false, 'alive query/report');
  assert.equal(isOscNotification('99;i=one:p=?;'), false, 'capability query/report');
  assert.equal(isOscNotification('8;;https://example.com'), false, 'unrelated OSC');
});

// TC-PL4i is deliberately chunked at awkward byte boundaries: PTY delivery has no obligation to
// align with escape-sequence boundaries, and background-tab notifications must still be reliable.
test('TC-PL4i streaming OSC notification parser handles splits, terminators, and multipart Kitty', () => {
  const p = createOscNotificationParser();
  assert.equal(p.write('plain output\x1b'), 0);
  assert.equal(p.write(']777;notify;Claude;Waiting'), 0);
  assert.equal(p.write('\x07tail'), 1, 'split OSC 777 with BEL');

  assert.equal(p.write('\x1b]9;Done\x1b\\'), 1, 'OSC 9 with ST');
  assert.equal(p.write('\x9d9;C1 form\x9c'), 1, '8-bit OSC/ST form');

  assert.equal(p.write('\x1b]99;i=n1:d=0;p=title;Build\x1b\\'), 0, 'multipart title is not complete');
  assert.equal(p.write('\x1b]99;i=n1:p=body;Finished\x1b\\'), 1, 'multipart final body notifies once');
  assert.equal(p.write('\x1b]99;i=n1:p=close;\x1b\\'), 0, 'close control does not re-notify');

  assert.equal(p.write('\x1b]9;one\x07noise\x1b]777;notify;two;body\x07'), 2,
    'multiple complete notifications in one chunk');
});

test('TC-T1 synchronized-output frame marker is TUI activity', () => {
  assert.equal(containsTuiRepaint('\x1b[?2026hframe\x1b[?2026l'), true);
});

test('TC-T2 two-argument cursor positioning and scroll regions are TUI activity', () => {
  assert.equal(containsTuiRepaint('\x1b[12;40H'), true, 'CUP');
  assert.equal(containsTuiRepaint('\x1b[1;24r'), true, 'DECSTBM');
});

test('TC-T3 multi-row cursor-up matches while ordinary one-row redraw does not', () => {
  assert.equal(containsTuiRepaint('\x1b[2A'), true);
  assert.equal(containsTuiRepaint('\x1b[5A'), true);
  assert.equal(containsTuiRepaint('\x1b[12A'), true);
  assert.equal(containsTuiRepaint('\x1b[1A'), false);
});

test('TC-T4 home, one-argument row movement, CHA, and SGR are not TUI activity', () => {
  assert.equal(containsTuiRepaint('\x1b[H'), false);
  assert.equal(containsTuiRepaint('\x1b[5H'), false);
  assert.equal(containsTuiRepaint('\x1b[;5H'), false);
  assert.equal(containsTuiRepaint('\x1b[51G'), false, 'CHA is intentionally excluded');
  assert.equal(containsTuiRepaint('\x1b[1;32mgreen\x1b[0m'), false);
});

test('TC-T5 append-only shell and progress output do not create TUI activity', () => {
  const plain = [
    '\x1b[1;34mREADME.md\x1b[0m package.json',
    'first prompt line\nsecond prompt line $ ',
    'progress 10%\rprogress 100%\n',
    '\x1b[?2004hbracketed paste enabled',
  ].join('\n');
  assert.equal(containsTuiRepaint(plain), false);
});

test('TC-T6 tracker detects a repaint sequence split across PTY chunks', () => {
  const tracker = createTuiActivityTracker({ decayMs: 1000, now: () => 100 });
  assert.equal(tracker.write('prefix\x1b[12;'), false);
  assert.equal(tracker.write('40Hframe'), true);
});

test('TC-T7 tracker activity decays, re-arms, and resets deterministically', () => {
  let now = 1000;
  const tracker = createTuiActivityTracker({ decayMs: 10_000, now: () => now });
  assert.equal(tracker.write('\x1b[5A'), true);
  now = 10_999;
  assert.equal(tracker.active(), true);
  now = 11_000;
  assert.equal(tracker.active(), false, 'inactive at the decay boundary');
  now = 20_000;
  assert.equal(tracker.write('next frame \x1b[3A'), true, 'later frame re-arms');
  tracker.reset();
  assert.equal(tracker.active(), false);
});

test('TC-T8 real Claude Code startup capture is detected without alt-screen', () => {
  const capture = readFileSync(fixturePath('claude-code-2.1.224.raw'), 'latin1');
  const multiRowCuu = [...capture.matchAll(/\x1b\[(?:[2-9]|\d{2,})A/g)].map((match) => match[0]);
  const chaCount = [...capture.matchAll(/\x1b\[\d+G/g)].length;
  assert.equal(capture.includes('\x1b[?1049h'), false, 'Claude stays on the normal buffer');
  assert.ok(multiRowCuu.length > 0, 'capture contains the load-bearing multi-row CUU shape');
  assert.equal(containsTuiRepaint(multiRowCuu.join('')), true,
    'the CUU alternative detects Claude independently of its other control sequences');
  assert.ok(chaCount > multiRowCuu.length, 'CHA is common but remains deliberately excluded');
  assert.equal(containsTuiRepaint(capture), true);
});

test('TC-T9 real plain zsh capture does not create TUI activity', () => {
  const capture = readFileSync(fixturePath('plain-zsh.raw'), 'latin1');
  assert.equal(containsTuiRepaint(capture), false);
  const tracker = createTuiActivityTracker();
  assert.equal(tracker.write(capture), false);
});

// TC-PL5 custom plugin registers and matches; unregister works
test('TC-PL5 custom plugin + unregister', () => {
  const r = new PluginRegistry();
  let clicked = null;
  const un = r.registerLink({ name: 'jira', regex: /\b[A-Z]+-\d+\b/g, onClick: (t) => { clicked = t; } });
  let m = r.findMatches('fix ABC-123 now');
  assert.equal(m.length, 1);
  const match = m[0];
  assert.ok(match);
  assert.equal(match.text, 'ABC-123');
  match.plugin.onClick(match.text, { openExternal() {}, setStatus() {} });
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
  assert.equal(m[0]?.plugin.name, 'special');
});

// TC-PL8 matches returned sorted by start, non-overlapping
test('TC-PL8 sorted non-overlapping', () => {
  const r = reg();
  const m = r.findMatches('a /x/y b https://z.co c ~/d');
  const starts = m.map((x) => x.start);
  assert.deepEqual(starts, [...starts].sort((a, b) => a - b));
  for (let i = 1; i < m.length; i++) {
    const current = m[i], previous = m[i - 1];
    assert.ok(current && previous && current.start >= previous.end, 'no overlap');
  }
});

// TC-PL9 tab-kind registry: register/create/has/unregister (polymorphic tabs, §14/§15)
test('TC-PL9 tab-kind registry', () => {
  const r = new PluginRegistry();
  assert.equal(r.hasTabKind('terminal'), false);
  let created = null;
  const un = r.registerTabKind({
    kind: 'markdown',
    create: (spec) => {
      created = spec;
      return {
        kind: 'markdown', mounted: false, mount() {}, onData() {}, fit() { return null; },
        resize() {}, focus() {}, readBuffer() { return ''; }, dispose() {},
      };
    },
  });
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
  assert.throws(() => r.registerTabKind({ kind: 'x' } as never), /create/);
  assert.throws(() => r.registerTabKind({ create() {} } as never), /kind/);
});
