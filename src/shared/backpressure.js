'use strict';
// Backpressure accounting (DESIGN.md §4.1). Lives in MAIN; driven by renderer write-ACKs.
// xterm.js silently DISCARDS past a ~50MB internal buffer, so we must never outrun it:
// track unacked bytes, pause the pty over HIGH, resume under LOW. Never drops data.

const DEFAULT_HIGH = 100 * 1024; // 100 KB (xterm guide: keep <= 500K)
const DEFAULT_LOW = 10 * 1024;   // 10 KB

class Backpressure {
  // onPause/onResume are called at the edges only (no flapping).
  constructor({ high = DEFAULT_HIGH, low = DEFAULT_LOW, onPause, onResume } = {}) {
    if (low >= high) throw new Error('LOW watermark must be < HIGH');
    this.high = high;
    this.low = low;
    this.onPause = onPause || (() => {});
    this.onResume = onResume || (() => {});
    this.unacked = 0;
    this.paused = false;
  }

  // Call when bytes are dispatched toward the renderer.
  onData(byteLength) {
    this.unacked += byteLength;
    if (!this.paused && this.unacked >= this.high) {
      this.paused = true;
      this.onPause();
    }
    return this.paused;
  }

  // Call when the renderer ACKs having written `byteLength`.
  ack(byteLength) {
    this.unacked -= byteLength;
    if (this.unacked < 0) this.unacked = 0;
    if (this.paused && this.unacked <= this.low) {
      this.paused = false;
      this.onResume();
    }
    return this.paused;
  }
}

module.exports = { Backpressure, DEFAULT_HIGH, DEFAULT_LOW };
