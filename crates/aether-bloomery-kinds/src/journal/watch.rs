//! Long-poll watch on the journal head (ADR-0226 decision 10).

use alloc::string::String;

/// Watch the journal head, answered once it passes `after`.
///
/// A watch whose `after` is already behind the head is answered at once
/// with the current head. Otherwise the watch is parked and answered
/// exactly once, with the head as it stands after the first committed
/// write that moves the head past `after` — `MoveHead`, `Publish`, or
/// `AppendRecords`. A write that conflicts on its fence or is refused
/// wakes nobody. Parked watches are bounded; a watch beyond the bound is
/// refused at once, never parked. A caller that vanishes leaves its
/// parked entry until the next wake, and the reply to it is warn-dropped
/// like any unresolved recipient; there is no `Unwatch`. Watches do not
/// survive the journal actor's teardown or restart — a caller re-issues
/// `WatchHead` against the new instance. Duplicate watches from one
/// caller are independent entries, each answered once.
#[aether_data::kind(name = "aether.bloomery.journal.watch_head", copy, eq)]
pub struct WatchHead {
    /// Exclusive sequence boundary; the watch answers once the head passes this.
    pub after: u64,
}

/// Result of one [`WatchHead`].
#[aether_data::kind(name = "aether.bloomery.journal.watch_head_result", eq)]
pub enum WatchHeadResult {
    /// The journal head when the answer was sent; `head > after` always
    /// holds. May be well past `after + 1`, because one batch can append
    /// many entries.
    Advanced {
        /// Journal head when the answer was sent.
        head: u64,
    },
    /// A failed head read, or a full watcher table when the watch would
    /// otherwise have parked.
    Err {
        /// Human-readable failure.
        message: String,
    },
}
