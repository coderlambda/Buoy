//! ReplyChannel: request/response correlation over a tmux control-mode command stream
//! (DESIGN.md §12). Port of src/main/replyChannel.js. tmux emits exactly one %begin..%end reply
//! block per command, in submission order, plus one unsolicited handshake block at connect.
//!
//! In Rust we can't stash arbitrary closures with captured backend state as ergonomically as JS,
//! so the queue holds a `ReplyKind` tag describing what to do with each reply. The backend
//! matches on the tag when a reply arrives — this keeps the positional-correlation invariant
//! (one entry per command, FIFO) while staying borrow-checker friendly.

use std::collections::VecDeque;

/// What a given command's reply should be used for.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyKind {
    /// Fire-and-forget: drop the (usually empty) ack.
    Ignore,
    /// A `list-panes` topology listing: reconcile the registry.
    Topology,
    /// A `capture-pane` scrollback back-fill, painted into the given window (None = active).
    Capture { window: Option<String> },
    /// The cursor-position query paired with a completed capture. `capture-pane` returns only
    /// cells, not the pane cursor; retain the captured rows until this second FIFO reply arrives.
    CaptureCursor { window: String, body: Vec<String> },
}

pub struct ReplyChannel {
    queue: VecDeque<ReplyKind>,
    started: bool,
}

impl Default for ReplyChannel {
    fn default() -> Self { Self::new() }
}

impl ReplyChannel {
    pub fn new() -> Self {
        ReplyChannel { queue: VecDeque::new(), started: false }
    }

    /// Seed the handler for tmux's unsolicited connect handshake block. Idempotent.
    pub fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.queue.push_back(ReplyKind::Ignore);
    }

    /// Register the reply kind for a command about to be written (FIFO).
    pub fn expect(&mut self, kind: ReplyKind) {
        self.queue.push_back(kind);
    }

    /// Dequeue the kind for the reply block that just arrived. `None` => an unexpected extra reply.
    pub fn take(&mut self) -> Option<ReplyKind> {
        self.queue.pop_front()
    }

    #[allow(dead_code)] // used by tests; kept as part of the channel's inspection API
    pub fn pending(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_rc2_fifo_order() {
        let mut rc = ReplyChannel::new();
        rc.expect(ReplyKind::Topology);
        rc.expect(ReplyKind::Capture { window: Some("@1".into()) });
        assert_eq!(rc.take(), Some(ReplyKind::Topology));
        assert_eq!(rc.take(), Some(ReplyKind::Capture { window: Some("@1".into()) }));
        assert_eq!(rc.take(), None);
    }

    #[test]
    fn tc_rc3_handshake_seed() {
        let mut rc = ReplyChannel::new();
        rc.start();
        rc.expect(ReplyKind::Topology);
        // first reply consumed by the handshake seed, NOT the topology command
        assert_eq!(rc.take(), Some(ReplyKind::Ignore));
        assert_eq!(rc.take(), Some(ReplyKind::Topology));
    }

    #[test]
    fn tc_rc4_start_idempotent() {
        let mut rc = ReplyChannel::new();
        rc.start();
        rc.start();
        assert_eq!(rc.pending(), 1);
    }

    #[test]
    fn tc_rc5_fire_and_forget_consumes_slot() {
        let mut rc = ReplyChannel::new();
        rc.expect(ReplyKind::Ignore); // fire-and-forget
        rc.expect(ReplyKind::Capture { window: None });
        assert_eq!(rc.take(), Some(ReplyKind::Ignore));
        assert_eq!(rc.take(), Some(ReplyKind::Capture { window: None }));
    }

    #[test]
    fn tc_rc6_extra_reply_none() {
        let mut rc = ReplyChannel::new();
        assert_eq!(rc.take(), None);
    }
}
