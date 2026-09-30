//! Step spans: one `tracing` span per ticketed command the shell performs.
//!
//! The shell opens a span under [`STEP_TARGET`] when it sends a ticketed
//! command and closes it when the reply that takes the ticket back arrives,
//! before the core sees the reply. A span therefore measures one step's wall
//! time from send to reply: the wait in the recipient's queue, the
//! recipient's work, and the reply's delivery. The core stays sans-io: it
//! mints the tickets, and the shell alone reads the clock through the
//! subscriber.
//!
//! Each span is named for its step, at `DEBUG`:
//!
//! - `read_artifact` (`purpose`: `bundle`, `reactor_set`, `destination`,
//!   `fetch`, or `until`): one journal artifact read, the declare read of a
//!   bundle's wasm among them;
//! - `read_closure`: one input's transitive closure read;
//! - `load` (`bundle`): a bundle's publish through its root's spawn;
//! - `invoke` (`bundle`, `seq`, `closure`, the injected artifact count): one
//!   program run on a loaded root;
//! - `append`: one fenced journal append, its fsync included;
//! - `read_events`: one page of journal entries, read at recovery and to
//!   route live entries;
//! - `read_artifacts`: the artifacts a routing page's entries cite;
//! - `warm`, `evaluate`, and `status` (`bundle`): a reactor root's warmup,
//!   one live entry, and its status query;
//! - `fetch` and `run_workspace`: a program's relayed `Http` and `Workspace`
//!   calls.
//!
//! The head watch is not spanned: it parks until the head moves, so its
//! duration is idle time, not work. The clock's handler turn is the `tick`
//! span, which the fired timers' derivation runs inside.
//!
//! A span no subscriber enables is never stored, so an unobserved driver
//! pays one callsite-interest check per command. A span still open when the
//! driver drops closes then, as its reply would have.

use std::collections::HashMap;

use tracing::Span;

use super::core::{ArtifactRead, TicketId};

/// The `tracing` target every step span is opened under, for a subscriber
/// to filter on.
pub const STEP_TARGET: &str = "aether.bloomery.step";

/// The open step spans, keyed by the raw id of the ticket each step's reply
/// takes back.
#[derive(Default)]
pub struct StepSpans {
    open: HashMap<u64, Span>,
}

impl StepSpans {
    /// Keep `span` open until [`Self::close`] takes back `ticket`, unless no
    /// subscriber enabled it.
    pub(crate) fn open(&mut self, ticket: impl TicketId, span: Span) {
        if !span.is_disabled() {
            self.open.insert(ticket.id(), span);
        }
    }

    /// Close the span `ticket`'s step opened, if one is open.
    pub(crate) fn close(&mut self, ticket: impl TicketId) {
        self.open.remove(&ticket.id());
    }
}

/// The `purpose` field of a `read_artifact` span: why the core read the
/// artifact, or `unknown` for a read the core did not record.
pub const fn purpose(read: Option<&ArtifactRead>) -> &'static str {
    match read {
        Some(ArtifactRead::Bundle(_)) => "bundle",
        Some(ArtifactRead::ReactorSet(_)) => "reactor_set",
        Some(ArtifactRead::SetHeadsDestination) => "destination",
        Some(ArtifactRead::Fetch(_)) => "fetch",
        Some(ArtifactRead::Until(_)) => "until",
        None => "unknown",
    }
}
