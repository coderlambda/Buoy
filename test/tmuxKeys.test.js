'use strict';
const { test } = require('node:test');
const assert = require('node:assert');
const { escapeLiteral, encodeSendKeys } = require('../src/main/tmuxKeys');

// TC-TK1 plain text -> one send-keys -l addressed to the target
test('TC-TK1 plain text becomes one send-keys -l', () => {
  assert.deepEqual(encodeSendKeys('ls', '@2'), ['send-keys -t @2 -l "ls"']);
});

// TC-TK2 a trailing newline becomes a separate Enter key (NOT a literal \n)
test('TC-TK2 newline becomes Enter, text via -l', () => {
  assert.deepEqual(encodeSendKeys('ls\n', '@2'),
    ['send-keys -t @2 -l "ls"', 'send-keys -t @2 Enter']);
});

// TC-TK3 multiple lines: each break is its own Enter, interleaved with text runs
test('TC-TK3 multi-line input interleaves text and Enter', () => {
  assert.deepEqual(encodeSendKeys('a\nb', '@0'),
    ['send-keys -t @0 -l "a"', 'send-keys -t @0 Enter', 'send-keys -t @0 -l "b"']);
});

// TC-TK4 CRLF and CR are treated as line breaks too
test('TC-TK4 CRLF / CR are line breaks', () => {
  assert.deepEqual(encodeSendKeys('x\r\n', '@1'),
    ['send-keys -t @1 -l "x"', 'send-keys -t @1 Enter']);
  assert.deepEqual(encodeSendKeys('x\r', '@1'),
    ['send-keys -t @1 -l "x"', 'send-keys -t @1 Enter']);
});

// TC-TK5 works with a pane target too (@N or %N)
test('TC-TK5 accepts a pane target', () => {
  assert.deepEqual(encodeSendKeys('hi', '%5'), ['send-keys -t %5 -l "hi"']);
});

// TC-TK6 escaping: backslash, quote, tab, ESC, and other control bytes -> tmux C-escapes
test('TC-TK6 escapeLiteral escapes metacharacters and control bytes', () => {
  assert.equal(escapeLiteral('a\\b'), 'a\\\\b');       // backslash -> \\
  assert.equal(escapeLiteral('say "hi"'), 'say \\"hi\\"'); // quote -> \"
  assert.equal(escapeLiteral('a\tb'), 'a\\tb');        // tab -> \t
  assert.equal(escapeLiteral('\x1b[A'), '\\e[A');      // ESC -> \e
  assert.equal(escapeLiteral('\x01'), '\\001');        // other control -> octal
});

// TC-TK7 empty string / empty runs produce no commands
test('TC-TK7 empty input yields no commands', () => {
  assert.deepEqual(encodeSendKeys('', '@0'), []);
});
