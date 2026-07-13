'use strict';
// Pure encoding of shell input into tmux control-mode `send-keys` commands (DESIGN.md §12).
// No IO, no backend state — just data -> command strings, so it is fully unit-testable.
//
// Two verified gotchas this encodes:
//  - Shell input MUST go through `send-keys`, addressed to a target (a window @N or pane %N;
//    tmux resolves @N to the window's active pane). Writing raw bytes to the -CC stream is
//    parsed as a tmux COMMAND, not shell input.
//  - Enter/Return must be the KEY name "Enter", NOT a literal "\n" via -l (verified: `-l "x\n"`
//    does not submit the line; `-l "x" Enter` does). So we split on line breaks and emit text
//    chunks via `-l` and each break as a separate `Enter` key.

// Escape a text chunk for tmux's double-quoted `send-keys -l` argument. tmux parses the quoted
// string, so backslash/quote and control bytes must be escaped as C-style escapes it understands.
function escapeLiteral(part) {
  let s = '';
  for (let i = 0; i < part.length; i++) {
    const c = part[i], code = part.charCodeAt(i);
    if (c === '\\') s += '\\\\';
    else if (c === '"') s += '\\"';
    else if (c === '\t') s += '\\t';
    else if (c === '\x1b') s += '\\e';
    else if (code < 0x20) s += '\\' + code.toString(8).padStart(3, '0');
    else s += c;
  }
  return s;
}

// Encode `data` into the ordered list of `send-keys` command lines needed to reproduce it at
// `target` (a window "@N" or pane "%N"). Text runs become `-l "..."`; line breaks become `Enter`.
function encodeSendKeys(data, target) {
  const out = [];
  const parts = String(data).split(/(\r\n|\r|\n)/);
  for (const part of parts) {
    if (part === '') continue;
    if (part === '\r' || part === '\n' || part === '\r\n') {
      out.push(`send-keys -t ${target} Enter`);
    } else {
      out.push(`send-keys -t ${target} -l "${escapeLiteral(part)}"`);
    }
  }
  return out;
}

module.exports = { escapeLiteral, encodeSendKeys };
