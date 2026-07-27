'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { decodeOsc52 } = require('../ui/terminalTab');

// OSC 52 is the standard "set the system clipboard" escape: ESC ] 52 ; <sel> ; <base64> ST.
// xterm.js ignores it by default, so the terminal tab opts in and routes the decoded text to the
// system clipboard. decodeOsc52 is the pure decode step (base64 -> UTF-8).
const b64 = (s) => Buffer.from(s, 'utf8').toString('base64');

// TC-CB1 decodes a normal clipboard-set payload ("c;<base64>")
test('TC-CB1 decodes OSC 52 clipboard payload', () => {
  assert.equal(decodeOsc52('c;' + b64('hello world')), 'hello world');
  // selection field can be p/q/s/0-7 too — all treated the same
  assert.equal(decodeOsc52('p;' + b64('primary')), 'primary');
  // multi-line selection (what you'd copy out of a Claude Code session)
  const multi = 'line1\nline2\n  indented';
  assert.equal(decodeOsc52('c;' + b64(multi)), multi);
});

// TC-CB2 UTF-8 (multibyte) round-trips
test('TC-CB2 decodes multibyte UTF-8', () => {
  const s = 'café — 日本語 ✳';
  assert.equal(decodeOsc52('c;' + b64(s)), s);
});

// TC-CB3 a clipboard READ request ("?" data) is refused (returns '') so a remote program can't
// exfiltrate the local clipboard.
test('TC-CB3 refuses clipboard read request', () => {
  assert.equal(decodeOsc52('c;?'), '');
});

// TC-CB4 malformed / empty payloads return '' (never throw)
test('TC-CB4 malformed payloads return empty', () => {
  assert.equal(decodeOsc52(''), '');
  assert.equal(decodeOsc52('c;'), '');
  assert.equal(decodeOsc52(null), '');
  // a payload with no selection field still decodes the base64
  assert.equal(decodeOsc52(b64('nofield')), 'nofield');
});
