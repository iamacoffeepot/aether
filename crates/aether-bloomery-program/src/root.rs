//! The program bundle root's state: its table plus the live-seq table.
//!
//! The root the `bundle` generator generates is a thin shell over [`Root`]: the
//! shell makes the actor calls (`spawn_inline_child`, `send`, `reply_to`,
//! `despawn_inline_child`) while this module owns the program table, the
//! rejections, the source-matched finish, and the per-name dispatch.

use alloc::collections::BTreeMap;

use aether_bloomery_kinds::{Invoke, Invoked};
use aether_data::MailboxId;

use crate::declare::Program;
use crate::invoke::invoke;
use crate::kinds::Detail;

/// One program of a bundle: its `NAME` and its monomorphized `invoke::<P>`.
#[derive(Clone, Copy)]
pub struct ProgramEntry {
    name: &'static str,
    run: fn(Invoke) -> Invoked,
}

impl ProgramEntry {
    /// The entry for `P`: `P::NAME` plus `invoke::<P>`.
    #[must_use]
    pub const fn of<P: Program>() -> Self {
        Self { name: P::NAME, run: invoke::<P> }
    }

    /// The program's `NAME`, unique within its bundle.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// A bundle's programs: non-empty, names unique. Proved by the `bundle` generator at expansion.
pub struct ProgramTable {
    entries: &'static [ProgramEntry],
}

impl ProgramTable {
    /// The bundle's programs in export order.
    #[must_use]
    pub const fn entries(&self) -> &'static [ProgramEntry] {
        self.entries
    }
}

/// The only constructor. Hidden; reached as `__macro_internals::program_table` by generated code.
///
/// Does not check and cannot fail or panic: the generator has already refused an empty set
/// and duplicate names before it emits this call.
#[doc(hidden)]
#[must_use]
pub const fn program_table(entries: &'static [ProgramEntry]) -> ProgramTable {
    ProgramTable { entries }
}

/// Run the entry named by `invoke.program()`; `Invoked::Rejected` (`"unknown program"`) when none is.
#[must_use]
pub fn dispatch(table: &ProgramTable, invoke: Invoke) -> Invoked {
    let Some(entry) = table.entries().iter().find(|entry| entry.name() == invoke.program().as_str()) else {
        return Invoked::Rejected { seq: invoke.seq(), reason: Detail::new("unknown program") };
    };
    (entry.run)(invoke)
}

/// One live invocation: the spawned child's alias plus the shell's handle.
struct Live<H> {
    child: MailboxId,
    handle: H,
}

/// The program role's state: the bundle's table plus the live-seq table.
pub struct Root<H> {
    table: &'static ProgramTable,
    live: BTreeMap<u64, Live<H>>,
}

impl<H> Root<H> {
    /// Empty live set over `table`. Infallible: the table invariant is proved at compile time.
    #[must_use]
    pub const fn new(table: &'static ProgramTable) -> Self {
        Self { table, live: BTreeMap::new() }
    }

    /// Check `invoke` against the table and the live set.
    ///
    /// # Errors
    ///
    /// `Invoked::Rejected` for an unknown program or a seq that is already live.
    #[must_use = "start or fail the admission; reply the rejection"]
    pub fn admit(&mut self, invoke: &Invoke) -> Result<Admission<'_, H>, Invoked> {
        let seq = invoke.seq();
        if !self.table.entries().iter().any(|entry| entry.name() == invoke.program().as_str()) {
            return Err(Invoked::Rejected { seq, reason: Detail::new("unknown program") });
        }
        if self.live.contains_key(&seq) {
            return Err(Invoked::Rejected { seq, reason: Detail::new("seq already live") });
        }
        Ok(Admission { root: self, seq })
    }

    /// Remove and return `(child, handle)` for `invoked`'s seq, only when that seq
    /// is live and `source` is its child or absent. Otherwise changes nothing.
    #[must_use]
    pub fn finish(&mut self, invoked: &Invoked, source: Option<MailboxId>) -> Option<(MailboxId, H)> {
        let seq = match invoked {
            Invoked::Completed { seq, .. } | Invoked::Refused { seq, .. } | Invoked::Rejected { seq, .. } => *seq,
        };
        let live = self.live.remove(&seq)?;
        if let Some(source) = source
            && live.child != source
        {
            self.live.insert(seq, live);
            return None;
        }
        Some((live.child, live.handle))
    }
}

/// Permission to start one admitted seq. No public constructor; borrows its root.
pub struct Admission<'r, H> {
    root: &'r mut Root<H>,
    seq: u64,
}

impl<H> Admission<'_, H> {
    /// The admitted seq.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Record the seq live with its spawned child and the shell's handle.
    pub fn start(self, child: MailboxId, handle: H) {
        self.root.live.insert(self.seq, Live { child, handle });
    }

    /// Release the seq unrecorded; `Invoked::Rejected` (`"failed to spawn invocation"`).
    #[must_use]
    pub fn spawn_failed(self) -> Invoked {
        Invoked::Rejected { seq: self.seq, reason: Detail::new("failed to spawn invocation") }
    }
}
