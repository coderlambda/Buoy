'use strict';
// ReplyChannel: the request/response protocol over a tmux control-mode (`-CC`) command stream
// (DESIGN.md §12). tmux emits exactly ONE `%begin..%end` reply block per command we send, in
// submission order, plus ONE unsolicited block at connect (the handshake). This object owns that
// correlation so the backend never has to guess which reply belongs to which command.
//
// Contract:
//  - Construct with a `write(line)` sink (writes one command line to the pty, no newline needed
//    beyond what we add) and call `start()` once the stream is live — start() seeds a handler for
//    the unsolicited handshake block so every later command stays aligned with its own reply.
//  - `send(line, handler?)` writes a command and enqueues its reply handler (FIFO). Fire-and-
//    forget commands omit the handler (their usually-empty ack is dropped).
//  - `onReply(ev)` is fed each parsed reply event; it invokes the head handler. If the queue is
//    empty (an unexpected extra reply), it returns false so the caller can surface it.
//
// Correlation is POSITIONAL, not content-based: an earlier design matched replies by content and
// desynced when a fresh window's capture reply was empty (indistinguishable from a command ack),
// so a later capture painted into the wrong tab. One-handler-per-command is the protocol's real
// contract and immune to empty bodies.

class ReplyChannel {
  constructor(write) {
    this._write = write;
    this._queue = [];      // FIFO of reply handlers, one per command (+ the handshake seed)
    this._started = false;
  }

  // Seed the handler for tmux's unsolicited connect handshake block (verified: exactly one
  // `%begin` arrives before we send anything). Idempotent.
  start() {
    if (this._started) return;
    this._started = true;
    this._queue.push(() => {});
  }

  // Send a command line and register its reply handler (defaults to a no-op ack).
  send(line, handler) {
    this._queue.push(typeof handler === 'function' ? handler : (() => {}));
    this._write(line + '\n');
  }

  // Dispatch a parsed reply event to the head handler. Returns true if a handler consumed it,
  // false if the queue was empty (unexpected extra reply — the caller decides what to do).
  onReply(ev) {
    const handler = this._queue.shift();
    if (!handler) return false;
    handler(ev);
    return true;
  }

  get pending() { return this._queue.length; }
}

module.exports = { ReplyChannel };
