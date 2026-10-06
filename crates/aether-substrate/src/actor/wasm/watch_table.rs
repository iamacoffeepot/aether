//! One wasm instance's watch table (ADR-0079 §8): the registrations the host
//! holds for a guest that watches other actors, and the watch ids that name
//! them.
//!
//! - **Groups and rows.** A group is one watcher and target. The watcher is
//!   the guest's own mailbox or the alias of one of its inline children; the
//!   target is the actor watched. A group holds one row per watched type the
//!   pair was watched through, each with the id `watch_p32` returned for it
//!   and the watched type's tag.
//! - **One registration per group.** A registered group holds the one
//!   [`MonitorHandle`] for its pair, however many rows it has, so one
//!   departure posts one notice to the watcher. The handle drops with the
//!   group's last row, which deregisters.
//! - **Waiting groups.** A group whose watcher has no published route yet (an
//!   inline child watching from its own `wire`, before the registry owner
//!   publishes its alias) or that a held candidate opened is kept without a
//!   registration. It registers once its watcher is addressable, and its
//!   target is then noticed at once if it closed meanwhile (ADR-0247 rule 6).
//! - **A watch is unique per watcher, target, and watched type.** A second
//!   watch of a standing triple finds its row and returns the same id.
//! - **A candidate changes nothing before commit** (ADR-0241 §7). Rows a
//!   held candidate adds are marked, and a carried row it releases is only
//!   recorded as released. A commit applies both; an abort drops the marked
//!   rows and forgets the recorded releases, leaving the kept guest's watches
//!   as they stood.
//! - **No lock.** Only the owning actor's handler touches the table. It lives
//!   in the `Store` data (`ComponentCtx`) that the slot's actor `Mutex`
//!   guards, one handler at a time.
//! - **One mailbox.** A republish moves the table from the kept guest to its
//!   candidate and back on an abort, so a registration, which is keyed by the
//!   watcher's mailbox, stands across it untouched.
//! - **Teardown.** The table drops with its instance, and each registered
//!   group's handle releases its registration, with no guest code run: a
//!   closed, trapped, or discarded instance leaves nothing registered.
//!   Dropping a table that still holds rows logs one debug-level count.

use std::collections::hash_map::Entry;
use std::mem;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::actor::monitor::MonitorHandle;
use crate::mail::MailboxId;

/// A watcher and the target it watches: the key of one group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WatchPair {
    pub watcher: MailboxId,
    pub target: MailboxId,
}

/// Which guest made a row: the one whose watches stand, or a held candidate
/// whose rows count only once it commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowOrigin {
    Standing,
    Candidate,
}

/// One watch: its id and the tag of the type its target was watched through.
struct WatchRow {
    id: u64,
    watched: u64,
    origin: RowOrigin,
}

impl WatchRow {
    fn stands(&self) -> bool {
        self.origin == RowOrigin::Standing
    }
}

/// How a new group opens: registered at once, or waiting for its watcher to
/// become addressable.
pub enum Opened {
    Registered(MonitorHandle),
    Waiting,
}

/// A group whose pair is registered: the handle and the rows it serves.
struct RegisteredGroup {
    // Held for its `Drop`, which deregisters the pair.
    _handle: MonitorHandle,
    rows: Vec<WatchRow>,
}

/// The watches one wasm instance holds. See the module docs.
#[derive(Default)]
pub struct WatchTable {
    registered: FxHashMap<WatchPair, RegisteredGroup>,
    waiting: FxHashMap<WatchPair, Vec<WatchRow>>,
    /// The group each live watch id sits in.
    pairs: FxHashMap<u64, WatchPair>,
    /// Carried rows a held candidate released, applied on commit.
    released: FxHashSet<u64>,
}

impl WatchTable {
    fn rows(&self, pair: WatchPair) -> Option<&Vec<WatchRow>> {
        self.registered.get(&pair).map(|group| &group.rows).or_else(|| self.waiting.get(&pair))
    }

    fn rows_mut(&mut self, pair: WatchPair) -> Option<&mut Vec<WatchRow>> {
        match self.registered.get_mut(&pair) {
            Some(group) => Some(&mut group.rows),
            None => self.waiting.get_mut(&pair),
        }
    }

    /// The id of the watch standing for `pair` through `watched`, if one
    /// does. A row a held candidate released no longer stands.
    fn standing(&self, pair: WatchPair, watched: u64) -> Option<u64> {
        let row = self.rows(pair)?.iter().find(|row| row.watched == watched && !self.released.contains(&row.id))?;
        Some(row.id)
    }

    /// Watch `pair` through `watched` and return the watch's id. A standing
    /// watch of that triple answers its own id and nothing else changes.
    /// Otherwise `mint` draws a new id and the row joins the pair's group,
    /// which `open` opens when the pair has none.
    pub fn watch(
        &mut self,
        pair: WatchPair,
        watched: u64,
        origin: RowOrigin,
        mint: impl FnOnce() -> u64,
        open: impl FnOnce() -> Opened,
    ) -> u64 {
        if let Some(id) = self.standing(pair, watched) {
            return id;
        }

        let id = mint();
        let row = WatchRow { id, watched, origin };
        self.pairs.insert(id, pair);
        if let Some(rows) = self.rows_mut(pair) {
            rows.push(row);
            return id;
        }

        match open() {
            Opened::Registered(handle) => {
                self.registered.insert(pair, RegisteredGroup { _handle: handle, rows: vec![row] });
            }
            Opened::Waiting => {
                self.waiting.insert(pair, vec![row]);
            }
        }
        id
    }

    /// End the watch `id`, answering whether one was there. Its group goes
    /// with its last row, and a registered group's handle with it.
    pub fn release(&mut self, id: u64) -> bool {
        let Some(pair) = self.pairs.remove(&id) else {
            return false;
        };
        self.released.remove(&id);

        if let Entry::Occupied(mut group) = self.registered.entry(pair) {
            group.get_mut().rows.retain(|row| row.id != id);
            if group.get().rows.is_empty() {
                group.remove();
            }
            return true;
        }
        if let Entry::Occupied(mut rows) = self.waiting.entry(pair) {
            rows.get_mut().retain(|row| row.id != id);
            if rows.get().is_empty() {
                rows.remove();
            }
        }
        true
    }

    /// A held candidate's release of `id`, answering whether a standing watch
    /// was there. A row the candidate made itself ends now. A carried row
    /// stays until [`Self::commit_candidate`] and stops counting as standing,
    /// so the kept guest still holds it if the candidate is discarded.
    pub fn record_release(&mut self, id: u64) -> bool {
        let Some(pair) = self.pairs.get(&id).copied() else {
            return false;
        };
        let carried = self.rows(pair).is_some_and(|rows| rows.iter().any(|row| row.id == id && row.stands()));
        if carried {
            return self.released.insert(id);
        }
        self.release(id)
    }

    /// End the watch a departure notice from `pair.target` to `pair.watcher`
    /// is for, through `watched`, and return its id. Only a registered group
    /// can have been sent a notice.
    pub fn end(&mut self, pair: WatchPair, watched: u64) -> Option<u64> {
        let id = self.registered.get(&pair)?.rows.iter().find(|row| row.watched == watched)?.id;
        self.release(id);
        Some(id)
    }

    /// Register each waiting group whose watcher `addressable` admits,
    /// through `register`. While `held`, a group made only of a candidate's
    /// rows keeps waiting: it registers when the candidate commits.
    pub fn register_waiting(
        &mut self,
        held: bool,
        addressable: impl Fn(MailboxId) -> bool,
        mut register: impl FnMut(WatchPair) -> MonitorHandle,
    ) {
        if self.waiting.is_empty() {
            return;
        }

        let ready: Vec<WatchPair> = self
            .waiting
            .iter()
            .filter(|(pair, rows)| {
                let stands = rows.iter().any(WatchRow::stands);
                let owed = stands || !held;
                owed && addressable(pair.watcher)
            })
            .map(|(pair, _)| *pair)
            .collect();
        for pair in ready {
            let Some(rows) = self.waiting.remove(&pair) else {
                continue;
            };
            self.registered.insert(pair, RegisteredGroup { _handle: register(pair), rows });
        }
    }

    /// Make a committed candidate's changes the table's own: apply the
    /// releases it recorded and clear the marks on the rows it added.
    pub fn commit_candidate(&mut self) {
        for id in mem::take(&mut self.released) {
            self.release(id);
        }
        let groups = self.registered.values_mut().map(|group| &mut group.rows).chain(self.waiting.values_mut());
        for row in groups.flatten() {
            row.origin = RowOrigin::Standing;
        }
    }

    /// Undo a discarded candidate's changes: drop the rows it added, and the
    /// groups made only of them, and forget the releases it recorded.
    pub fn discard_candidate(&mut self) {
        self.released.clear();
        let groups = self.registered.values().map(|group| &group.rows).chain(self.waiting.values());
        let added: Vec<u64> = groups.flatten().filter(|row| !row.stands()).map(|row| row.id).collect();
        for id in added {
            self.release(id);
        }
    }
}

impl Drop for WatchTable {
    fn drop(&mut self) {
        if self.pairs.is_empty() {
            return;
        }
        tracing::debug!(
            target: "aether_substrate::component",
            registrations = self.registered.len(),
            waiting = self.waiting.len(),
            watches = self.pairs.len(),
            "releasing watches still held at instance teardown",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;

    use super::*;
    use crate::actor::native::NativeBinding;
    use crate::mail::registry::Registry;
    use crate::testing::bare_substrate;

    const WATCHER: MailboxId = MailboxId(0x10);
    const TARGET: MailboxId = MailboxId(0x20);
    const PAIR: WatchPair = WatchPair { watcher: WATCHER, target: TARGET };
    const PROVIDER: u64 = 1;
    const AUDITOR: u64 = 2;

    /// A registry and a binding at [`WATCHER`] to register real handles
    /// through. A target with no slot is watched, so no route is needed.
    fn substrate() -> (Arc<Registry>, NativeBinding) {
        let (registry, mailer) = bare_substrate();
        (registry, NativeBinding::new_for_test(mailer, WATCHER))
    }

    fn registrations(registry: &Registry) -> usize {
        registry.actor_registry().monitor_count(TARGET)
    }

    /// An id source that counts how many ids it handed out.
    struct Ids(Cell<u64>);

    impl Ids {
        fn mint(&self) -> u64 {
            self.0.set(self.0.get() + 1);
            self.0.get()
        }

        fn minted(&self) -> u64 {
            self.0.get()
        }
    }

    fn watch(table: &mut WatchTable, binding: &NativeBinding, ids: &Ids, watched: u64, origin: RowOrigin) -> u64 {
        table.watch(
            PAIR,
            watched,
            origin,
            || ids.mint(),
            || Opened::Registered(MonitorHandle::register(binding, WATCHER, TARGET)),
        )
    }

    /// Catches a repeated watch making a second watch: a second id, a second
    /// row, or a second registration for a triple that already stands.
    #[test]
    fn a_repeated_watch_answers_the_standing_id_and_adds_nothing() {
        let (registry, binding) = substrate();
        let ids = Ids(Cell::new(0));
        let mut table = WatchTable::default();

        let first = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing);
        let second = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing);

        assert_eq!(first, second);
        assert_eq!(ids.minted(), 1, "the repeat draws no id");
        assert_eq!(registrations(&registry), 1);
        assert_eq!(table.end(PAIR, PROVIDER), Some(first));
        assert_eq!(table.end(PAIR, PROVIDER), None, "one row stood, so one departure ends one watch");
    }

    /// Catches a second registration for a pair watched through two types,
    /// and a handle dropped while a watched type still stands.
    #[test]
    fn two_watched_types_share_one_registration_until_the_last_ends() {
        let (registry, binding) = substrate();
        let ids = Ids(Cell::new(0));
        let mut table = WatchTable::default();

        let provider = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing);
        let auditor = watch(&mut table, &binding, &ids, AUDITOR, RowOrigin::Standing);
        assert_ne!(provider, auditor);
        assert_eq!(registrations(&registry), 1);

        assert_eq!(table.end(PAIR, PROVIDER), Some(provider));
        assert_eq!(registrations(&registry), 1, "the pair stays registered for its other watched type");
        assert_eq!(table.end(PAIR, AUDITOR), Some(auditor));
        assert_eq!(registrations(&registry), 0);
    }

    /// Catches an aborted candidate's `unwatch` ending the kept guest's
    /// watch, and a candidate's re-watch answering the id it released.
    #[test]
    fn a_candidates_release_waits_for_its_commit() {
        let (registry, binding) = substrate();
        let ids = Ids(Cell::new(0));
        let mut table = WatchTable::default();
        let kept = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing);

        assert!(table.record_release(kept));
        assert!(!table.record_release(kept), "a released watch is not there to release twice");
        assert_eq!(registrations(&registry), 1, "the row stays registered until the candidate commits");
        let again = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Candidate);
        assert_ne!(again, kept, "the released row no longer stands for its triple");

        table.discard_candidate();
        assert_eq!(watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing), kept);
        assert_eq!(registrations(&registry), 1);

        assert!(table.record_release(kept));
        table.commit_candidate();
        assert_eq!(registrations(&registry), 0, "a committed release ends the watch");
        assert_eq!(table.end(PAIR, PROVIDER), None);
    }

    /// Catches an aborted candidate's `watch` staying in the table, where a
    /// later departure would end a watch whose context no guest holds.
    #[test]
    fn discarding_a_candidate_drops_only_the_rows_it_added() {
        let (registry, binding) = substrate();
        let ids = Ids(Cell::new(0));
        let mut table = WatchTable::default();
        let other = WatchPair { watcher: WATCHER, target: MailboxId(0x30) };
        let kept = watch(&mut table, &binding, &ids, PROVIDER, RowOrigin::Standing);

        let added = watch(&mut table, &binding, &ids, AUDITOR, RowOrigin::Candidate);
        let waiting = table.watch(other, PROVIDER, RowOrigin::Candidate, || ids.mint(), || Opened::Waiting);
        table.register_waiting(
            true,
            |_| true,
            |_| unreachable!("a held candidate's own group registers only on commit"),
        );
        table.discard_candidate();

        assert!(!table.release(added));
        assert!(!table.release(waiting));
        assert_eq!(registrations(&registry), 1);
        assert_eq!(table.end(PAIR, PROVIDER), Some(kept));
    }
}
