'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { ControlModeParser, unescapeOutput, parseLayout, layoutPanes } = require('../src/shared/controlModeParser');

function collect(chunks) {
  const events = [];
  const p = new ControlModeParser((e) => events.push(e));
  (Array.isArray(chunks) ? chunks : [chunks]).forEach((c) => p.write(c));
  return events;
}

// TC-CM1 %output un-escaping (octal escapes -> raw bytes)
test('TC-CM1 unescapeOutput decodes octal escapes', () => {
  // \033 = ESC (0x1b), \015 = CR, \012 = LF
  assert.equal(unescapeOutput('\\033[1m'), '\x1b[1m');
  assert.equal(unescapeOutput('a\\015\\012b'), 'a\r\nb');
  assert.equal(unescapeOutput('plain text'), 'plain text');
  assert.equal(unescapeOutput('back\\\\slash'), 'back\\slash');
});

// TC-CM2 basic %output event routed with pane id, un-escaped data
test('TC-CM2 %output emits pane + decoded data', () => {
  const ev = collect('%output %0 hello\\015\\012world\r\n');
  assert.equal(ev.length, 1);
  assert.deepEqual(ev[0], { type: 'output', pane: '%0', data: 'hello\r\nworld' });
});

// TC-CM3 real captured stream (from tmux 3.5a on the live host) — session bootstrap
test('TC-CM3 parses the real bootstrap stream', () => {
  const raw =
    'P1000p%begin 1783717795 266 0\r\n%end 1783717795 266 0\r\n' +
    '%window-add @0\r\n%sessions-changed\r\n%session-changed $0 spec\r\n' +
    '%output %0 \\033[1m\\033[7m%\\033[27m\\033[1m\\033[0m\r\n';
  const ev = collect(raw);
  const types = ev.map((e) => e.type);
  assert.ok(types.includes('reply'), 'begin/end -> reply');
  assert.deepEqual(ev.find((e) => e.type === 'window-add'), { type: 'window-add', window: '@0' });
  assert.deepEqual(ev.find((e) => e.type === 'session-changed'), { type: 'session-changed', session: '$0', name: 'spec' });
  const out = ev.find((e) => e.type === 'output');
  assert.equal(out.pane, '%0');
  assert.ok(out.data.startsWith('\x1b[1m\x1b[7m%'), 'output un-escaped to real ESC bytes');
});

// TC-CM4 window lifecycle + pane change + rename (from capture)
test('TC-CM4 window add/rename/pane-changed', () => {
  const raw =
    '%session-window-changed $0 @1\r\n%window-add @1\r\n' +
    '%window-renamed @1 zsh\r\n%window-pane-changed @1 %2\r\n';
  const ev = collect(raw);
  assert.deepEqual(ev[0], { type: 'session-window-changed', session: '$0', window: '@1' });
  assert.deepEqual(ev[1], { type: 'window-add', window: '@1' });
  assert.deepEqual(ev[2], { type: 'window-renamed', window: '@1', name: 'zsh' });
  assert.deepEqual(ev[3], { type: 'window-pane-changed', window: '@1', pane: '%2' });
});

// TC-CM5 %begin/%error correlation (input-to-command-interpreter gotcha produced this)
test('TC-CM5 %begin/%error -> reply ok:false with body', () => {
  const raw = '%begin 1783717643 272 1\r\nparse error: unknown command: echo\r\n%error 1783717643 272 1\r\n';
  const ev = collect(raw);
  const begin = ev.find((e) => e.type === 'begin');
  assert.equal(begin.cmd, '272', 'begin exposes cmd# for capture correlation');
  const reply = ev.find((e) => e.type === 'reply');
  assert.equal(reply.ok, false);
  assert.equal(reply.cmd, '272');
  assert.deepEqual(reply.body, ['parse error: unknown command: echo']);
});

// TC-CM6 chunk boundaries: a control line split across two writes still parses once whole
test('TC-CM6 handles chunk splits', () => {
  const events = [];
  const p = new ControlModeParser((e) => events.push(e));
  p.write('%output %0 par');       // no newline yet -> nothing emitted
  assert.equal(events.length, 0);
  p.write('tial\r\n');             // completes the line
  assert.deepEqual(events, [{ type: 'output', pane: '%0', data: 'partial' }]);
});

// TC-CM7 %exit
test('TC-CM7 %exit emitted', () => {
  assert.deepEqual(collect('%exit\r\n'), [{ type: 'exit', reason: '' }]);
  assert.deepEqual(collect('%exit server exited\r\n'), [{ type: 'exit', reason: 'server exited' }]);
});

// TC-CM8 unknown lines are forward-compatible, not dropped silently
test('TC-CM8 unknown % line surfaces as unknown', () => {
  const ev = collect('%future-thing @9 stuff\r\n');
  assert.equal(ev[0].type, 'unknown');
  assert.match(ev[0].line, /future-thing/);
});

// TC-CM9 layout parsing (real capture: 419a,80x24,0,0[80x12,0,0,1,80x11,0,13,2])
test('TC-CM9 parseLayout builds a split tree', () => {
  const tree = parseLayout('419a,80x24,0,0[80x12,0,0,1,80x11,0,13,2]');
  assert.equal(tree.w, 80); assert.equal(tree.h, 24);
  assert.equal(tree.split, 'lr');            // [...] = left-right (stacked) split
  assert.equal(tree.children.length, 2);
  assert.equal(tree.children[0].pane, '%1');
  assert.equal(tree.children[1].pane, '%2');
  assert.deepEqual(layoutPanes(tree), ['%1', '%2']);
});

// TC-CM10 single-pane layout (leaf)
test('TC-CM10 parseLayout single pane', () => {
  const tree = parseLayout('abcd,80x24,0,0,0');
  assert.equal(tree.pane, '%0');
  assert.equal(tree.split, undefined);
  assert.deepEqual(layoutPanes(tree), ['%0']);
});

// TC-CM11 layout with a NON-zero pane id and coordinate zeros: only the trailing pane id is a
// pane. Regression: a naive regex once matched the x,y "0"s as pane "%0", polluting the
// pane->window map so every tab collapsed onto one pane (mixed output across tabs).
test('TC-CM11 parseLayout does not treat coordinate zeros as pane ids', () => {
  assert.deepEqual(layoutPanes(parseLayout('bfd4,203x51,0,0,171')), ['%171']);
  // and in a split, only the two real leaf pane ids come back (not the 0s in 0,0 / 0,26)
  assert.deepEqual(layoutPanes(parseLayout('a1b2,203x51,0,0[203x25,0,0,5,203x25,0,26,7]')), ['%5', '%7']);
});
