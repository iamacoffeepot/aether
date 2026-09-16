//! Registry lifecycle: lazy construction, incremental replay, failure, and poison.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNT_EMPTY: AtomicU64 = AtomicU64::new(0);

use aether_bloomery_journal::{Batch, Clock, Draft, Entry, Journal, Seq};
use aether_bloomery_kinds::{Head, Mode, OpaqueBytes, Program, ProgramName, RecordedHead, RecordedHeadMove};
use aether_bloomery_view::{Heads, View, ViewError, ViewRegistry};
use aether_data::Kind;

const PAGE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.registry_note")]
struct Note {
    n: u64,
}

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

struct Count {
    cursor: Seq,
    seen: u64,
    last_batch: usize,
    empty_gen: u64,
}

impl View for Count {
    type Error = Infallible;

    fn empty() -> Self {
        Self { cursor: Seq(0), seen: 0, last_batch: 0, empty_gen: COUNT_EMPTY.fetch_add(1, Ordering::Relaxed) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        self.last_batch = entries.len();
        self.seen += u64::try_from(entries.len()).expect("page fits u64");
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FoldFail;

impl fmt::Display for FoldFail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fold failed")
    }
}

impl Error for FoldFail {}

struct Failing;

impl View for Failing {
    type Error = FoldFail;

    fn empty() -> Self {
        Self
    }

    fn cursor(&self) -> Seq {
        Seq(0)
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        Err(FoldFail)
    }
}

struct Liar {
    cursor: Seq,
}

impl View for Liar {
    type Error = Infallible;

    fn empty() -> Self {
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct BornLate;

impl View for BornLate {
    type Error = Infallible;

    fn empty() -> Self {
        Self
    }

    fn cursor(&self) -> Seq {
        Seq(4)
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct PanicEmpty;

impl View for PanicEmpty {
    type Error = Infallible;

    fn empty() -> Self {
        panic!("empty")
    }

    fn cursor(&self) -> Seq {
        Seq(0)
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct PanicAdvance {
    cursor: Seq,
}

impl View for PanicAdvance {
    type Error = Infallible;

    fn empty() -> Self {
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        panic!("advance")
    }
}

fn memory() -> Result<Journal, Box<dyn Error>> {
    Ok(Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?)
}

fn append_notes(journal: &mut Journal, expect: Seq, count: u64) -> Result<Seq, Box<dyn Error>> {
    let mut batch = Batch::new();
    for n in 0..count {
        batch.push_draft(Draft::of(&Note { n }, None)?);
    }
    let range = journal.append(expect, &batch)?;
    Ok(Seq(range.end.0.saturating_sub(1)))
}

fn program(name: &str, intent: &str) -> Result<Program, Box<dyn Error>> {
    Ok(Program {
        name: ProgramName::new(name)?,
        input: OpaqueBytes::ID,
        result: OpaqueBytes::ID,
        mode: Mode::Pure,
        intent: intent.into(),
    })
}

#[test]
fn seq_zero_on_an_empty_journal_injects_empty_views() -> Result<(), Box<dyn Error>> {
    // Bug: Seq(0) is treated as a missing prefix and the callback is skipped or empty() is skipped.
    let mut registry = ViewRegistry::new(memory()?);
    let (cursor, seen) =
        registry.views::<(Heads, Count)>().at(Seq(0)).with(|(heads, count)| (heads.cursor(), count.seen))?;
    assert_eq!(cursor, Seq(0));
    assert_eq!(seen, 0);
    Ok(())
}

#[test]
fn absent_views_are_constructed_once_and_reused_at_the_same_prefix() -> Result<(), Box<dyn Error>> {
    // Bug: every with() rebuilds from empty, so last_batch equals the whole prefix again.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 3)?;
    let mut registry = ViewRegistry::new(journal);
    let first = registry.views::<Count>().at(head).with(|count| (count.seen, count.last_batch, count.empty_gen))?;
    let second = registry.views::<Count>().at(head).with(|count| (count.seen, count.last_batch, count.empty_gen))?;
    assert_eq!(first.0, 3);
    assert_eq!(first.1, 3);
    assert_eq!(second.0, 3);
    assert_eq!(first.2, second.2);
    Ok(())
}

#[test]
fn appends_between_callbacks_replay_only_the_new_prefix() -> Result<(), Box<dyn Error>> {
    // Bug: a later request rebuilds from Seq(0), or cannot append because the journal is borrowed.
    let mut journal = memory()?;
    let stored;
    {
        let mut batch = Batch::new();
        stored = batch.stage_encoded(&program("trim", "first")?)?;
        batch.push_event(&Head::<Program>::new("trim").move_to(stored), None)?;
        journal.append(Seq(0), &batch)?;
    }
    let mut registry = ViewRegistry::new(journal);
    let first = registry
        .views::<(Heads, Count)>()
        .at(Seq(1))
        .with(|(heads, count)| (heads.get(&Head::<Program>::new("trim")), count.seen, count.last_batch))?;
    assert_eq!(first, (Some(stored), 1, 1));

    let later;
    {
        let journal = registry.journal_mut();
        let mut batch = Batch::new();
        later = batch.stage_encoded(&program("trim", "second")?)?;
        batch.push_event(&Head::<Program>::new("trim").move_to(later), None)?;
        journal.append(Seq(1), &batch)?;
    }
    let second = registry
        .views::<(Heads, Count)>()
        .at(Seq(2))
        .with(|(heads, count)| (heads.get(&Head::<Program>::new("trim")), count.seen, count.last_batch))?;
    assert_eq!(second, (Some(later), 2, 1));
    Ok(())
}

#[test]
fn two_views_meet_at_the_same_exact_position() -> Result<(), Box<dyn Error>> {
    // Bug: one view is caught up to head while the other stops short, so injected cursors disagree.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 5)?;
    let mut registry = ViewRegistry::new(journal);
    let (heads_at, count_at) =
        registry.views::<(Heads, Count)>().at(head).with(|(heads, count)| (heads.cursor(), count.cursor()))?;
    assert_eq!(heads_at, head);
    assert_eq!(count_at, head);
    Ok(())
}

#[test]
fn duplicate_types_share_one_instance() -> Result<(), Box<dyn Error>> {
    // Bug: (Count, Count) constructs two folds, so they diverge after one advance.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 2)?;
    let mut registry = ViewRegistry::new(journal);
    let shared = registry
        .views::<(Count, Count)>()
        .at(head)
        .with(|(left, right)| (ptr::eq(left, right), left.seen, right.seen))?;
    assert_eq!(shared, (true, 2, 2));
    Ok(())
}

#[test]
fn eight_duplicate_slots_still_share_one_instance() -> Result<(), Box<dyn Error>> {
    // Bug: only low arities unique-ify TypeId, so an 8-tuple of one type constructs eight folds.
    let mut registry = ViewRegistry::new(memory()?);
    let shared = registry
        .views::<(Count, Count, Count, Count, Count, Count, Count, Count)>()
        .at(Seq(0))
        .with(|(a, _, _, _, _, _, _, h)| ptr::eq(a, h))?;
    assert!(shared);
    Ok(())
}

#[test]
fn multi_page_replay_then_a_tail_append_does_not_rebuild() -> Result<(), Box<dyn Error>> {
    // Bug: catch-up reads the whole log as one page, or a later append rebuilds from zero.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), u64::try_from(PAGE + 4)?)?;
    let mut registry = ViewRegistry::new(journal);
    let first = registry.views::<Count>().at(head).with(|count| (count.seen, count.last_batch))?;
    assert_eq!(first.0, u64::try_from(PAGE + 4)?);
    assert_eq!(first.1, 4);

    let next = {
        let journal = registry.journal_mut();
        append_notes(journal, head, 1)?
    };
    let second = registry.views::<Count>().at(next).with(|count| (count.seen, count.last_batch))?;
    assert_eq!(second, (u64::try_from(PAGE + 5)?, 1));
    Ok(())
}

#[test]
fn a_future_target_is_rejected_before_the_callback() -> Result<(), Box<dyn Error>> {
    // Bug: a target past head is treated as "read until empty" and the callback still runs.
    let mut registry = ViewRegistry::new(memory()?);
    let mut called = false;
    let error = registry
        .views::<Count>()
        .at(Seq(1))
        .with(|_| {
            called = true;
        })
        .expect_err("future target");
    assert!(!called);
    match error {
        ViewError::BeyondHead { target, head } => {
            assert_eq!(target, Seq(1));
            assert_eq!(head, Seq(0));
        }
        other => panic!("expected BeyondHead, got {other:?}"),
    }
    Ok(())
}

#[test]
fn a_target_behind_a_cached_cursor_is_rejected_before_mutating_siblings() -> Result<(), Box<dyn Error>> {
    // Bug: a rewind clones or rebuilds the cached view, or constructs a sibling before refusing.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 3)?;
    let mut registry = ViewRegistry::new(journal);
    registry.views::<Count>().at(head).with(|count| count.seen)?;
    let mut called = false;
    let error = registry
        .views::<(Count, Heads)>()
        .at(Seq(1))
        .with(|_| {
            called = true;
        })
        .expect_err("behind cursor");
    assert!(!called);
    assert!(
        matches!(error, ViewError::Behind { target, cursor, .. } if target == Seq(1) && cursor == head),
        "{error:?}"
    );
    let seen = registry.views::<Count>().at(head).with(|count| count.seen)?;
    assert_eq!(seen, 3);
    Ok(())
}

#[test]
fn irrelevant_entries_advance_every_requested_cursor() -> Result<(), Box<dyn Error>> {
    // Bug: Heads skips notes without advancing, so the next Heads request sees a gap.
    let mut journal = memory()?;
    append_notes(&mut journal, Seq(0), 1)?;
    let stored;
    {
        let mut batch = Batch::new();
        stored = batch.stage_encoded(&program("trim", "only")?)?;
        batch.push_event(&Head::<Program>::new("trim").move_to(stored), None)?;
        journal.append(Seq(1), &batch)?;
    }
    let mut registry = ViewRegistry::new(journal);
    let (binding, count_at, heads_at) = registry
        .views::<(Heads, Count)>()
        .at(Seq(2))
        .with(|(heads, count)| (heads.get(&Head::<Program>::new("trim")), count.cursor(), heads.cursor()))?;
    assert_eq!(binding, Some(stored));
    assert_eq!(count_at, Seq(2));
    assert_eq!(heads_at, Seq(2));
    Ok(())
}

#[test]
fn a_fold_error_poisons_that_view_and_skips_the_callback() -> Result<(), Box<dyn Error>> {
    // Bug: a failed advance is retried from a half-applied cursor, or the callback still runs.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 1)?;
    let mut registry = ViewRegistry::new(journal);
    let mut called = false;
    let error = registry
        .views::<Failing>()
        .at(head)
        .with(|_| {
            called = true;
        })
        .expect_err("fold failure");
    assert!(!called);
    assert!(matches!(error, ViewError::Advance { .. }), "{error:?}");
    let poisoned = registry.views::<Failing>().at(head).with(|_| ()).expect_err("still poisoned");
    assert!(matches!(poisoned, ViewError::Poisoned { last_trusted_cursor, .. } if last_trusted_cursor == Seq(0)));
    Ok(())
}

#[test]
fn a_successful_advance_with_the_wrong_cursor_poisons_the_view() -> Result<(), Box<dyn Error>> {
    // Bug: a lying cursor is trusted, so the next page looks like a gap or a duplicate.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 2)?;
    let mut registry = ViewRegistry::new(journal);
    let error = registry.views::<Liar>().at(head).with(|liar| -> Seq { liar.cursor() }).expect_err("cursor contract");
    match error {
        ViewError::CursorContract { expected, actual, .. } => {
            assert_eq!(expected, head);
            assert_eq!(actual, Seq(0));
        }
        other => panic!("expected CursorContract, got {other:?}"),
    }
    let poisoned = registry.views::<Liar>().at(head).with(|_| ()).expect_err("poisoned");
    assert!(matches!(poisoned, ViewError::Poisoned { .. }));
    Ok(())
}

#[test]
fn a_nonzero_empty_cursor_poisons_the_view() -> Result<(), Box<dyn Error>> {
    // Bug: a constructor that starts at Seq(4) is treated as already synchronized.
    let mut registry = ViewRegistry::new(memory()?);
    let error =
        registry.views::<BornLate>().at(Seq(0)).with(|view| -> Seq { view.cursor() }).expect_err("nonzero empty");
    match error {
        ViewError::NonzeroEmpty { cursor, .. } => assert_eq!(cursor, Seq(4)),
        other => panic!("expected NonzeroEmpty, got {other:?}"),
    }
    let poisoned = registry.views::<BornLate>().at(Seq(0)).with(|_| ()).expect_err("poisoned");
    assert!(matches!(poisoned, ViewError::Poisoned { .. }));
    Ok(())
}

#[test]
fn a_panic_in_empty_leaves_the_slot_poisoned() -> Result<(), Box<dyn Error>> {
    // Bug: a caught panic in empty() retries construction and can expose a half-built view.
    let mut registry = ViewRegistry::new(memory()?);
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = registry.views::<PanicEmpty>().at(Seq(0)).with(|_| ());
    }));
    assert!(caught.is_err());
    let error = registry.views::<PanicEmpty>().at(Seq(0)).with(|_| ()).expect_err("poisoned after empty panic");
    assert!(matches!(error, ViewError::Poisoned { last_trusted_cursor, .. } if last_trusted_cursor == Seq(0)));
    Ok(())
}

#[test]
fn a_panic_in_advance_leaves_the_slot_poisoned() -> Result<(), Box<dyn Error>> {
    // Bug: a caught panic in advance() leaves a half-applied view usable.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 1)?;
    let mut registry = ViewRegistry::new(journal);
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = registry.views::<PanicAdvance>().at(head).with(|_| ());
    }));
    assert!(caught.is_err());
    let error = registry.views::<PanicAdvance>().at(head).with(|_| ()).expect_err("poisoned after advance panic");
    assert!(matches!(error, ViewError::Poisoned { last_trusted_cursor, .. } if last_trusted_cursor == Seq(0)));
    Ok(())
}

#[test]
fn a_callback_panic_does_not_poison_synchronized_views() -> Result<(), Box<dyn Error>> {
    // Bug: unwinding from the callback poisons views that already reached the target.
    let mut journal = memory()?;
    let head = append_notes(&mut journal, Seq(0), 2)?;
    let mut registry = ViewRegistry::new(journal);
    let caught = catch_unwind(AssertUnwindSafe(|| {
        registry
            .views::<Count>()
            .at(head)
            .with(|_| panic!("callback"))
            .expect("synchronization succeeds before the callback");
    }));
    assert!(caught.is_err());
    let seen = registry.views::<Count>().at(head).with(|count| count.seen)?;
    assert_eq!(seen, 2);
    Ok(())
}

#[test]
fn replacing_the_journal_fails_closed_without_clearing_caches() -> Result<(), Box<dyn Error>> {
    // Bug: journal_mut replacement reuses old caches against the new log, or silently drops them.
    let mut first = memory()?;
    let head = append_notes(&mut first, Seq(0), 2)?;
    let mut registry = ViewRegistry::new(first);
    registry.views::<Count>().at(head).with(|count| count.seen)?;
    *registry.journal_mut() = memory()?;
    let mut called = false;
    let error = registry
        .views::<Count>()
        .at(Seq(0))
        .with(|_| {
            called = true;
        })
        .expect_err("binding");
    assert!(!called);
    assert!(matches!(error, ViewError::Binding));
    Ok(())
}

#[test]
fn into_journal_returns_the_same_allocation() -> Result<(), Box<dyn Error>> {
    // Bug: into_journal opens a new journal instead of returning the bound one.
    let journal = memory()?;
    let identity = journal.identity();
    let registry = ViewRegistry::new(journal);
    assert_eq!(registry.into_journal().identity(), identity);
    Ok(())
}

#[test]
fn journal_mut_is_the_borrow_program_apply_takes() -> Result<(), Box<dyn Error>> {
    // Compile-check: program::apply(&mut Journal, ...) is unchanged; the registry yields that borrow.
    fn apply_shape(journal: &mut Journal) -> Result<Seq, Box<dyn Error>> {
        Ok(journal.head()?)
    }

    let mut registry = ViewRegistry::new(memory()?);
    assert_eq!(apply_shape(registry.journal_mut())?, Seq(0));
    let head = append_notes(registry.journal_mut(), Seq(0), 1)?;
    assert_eq!(registry.journal().head()?, head);
    Ok(())
}

#[test]
fn filler_head_moves_across_pages_reuse_the_cached_heads_fold() -> Result<(), Box<dyn Error>> {
    // Bug: Heads is rebuilt per request, so a page of filler plus a later move disagrees with a fresh fold.
    let mut journal = memory()?;
    let filler;
    let trim;
    {
        let mut batch = Batch::new();
        filler = batch.stage_encoded(&program("fill", "page filler")?)?;
        trim = batch.stage_encoded(&program("trim", "live")?)?;
        for index in 0..PAGE {
            let name = format!("f{index:03}");
            batch.push_event(&RecordedHeadMove::new(RecordedHead::new(Program::ID, name)?, filler.digest()), None)?;
        }
        batch.push_event(&Head::<Program>::new("trim").move_to(trim), None)?;
        journal.append(Seq(0), &batch)?;
    }
    let mut registry = ViewRegistry::new(journal);
    let first = registry.views::<Heads>().at(Seq(u64::try_from(PAGE + 1)?)).with(|heads| {
        (heads.cursor(), heads.get(&Head::<Program>::new("trim")), heads.get(&Head::<Program>::new("f000")))
    })?;
    let later = {
        let journal = registry.journal_mut();
        let mut batch = Batch::new();
        let next = batch.stage_encoded(&program("trim", "after")?)?;
        batch.push_event(&Head::<Program>::new("trim").move_to(next), None)?;
        journal.append(Seq(u64::try_from(PAGE + 1)?), &batch)?;
        next
    };
    let second = registry.views::<Heads>().at(Seq(u64::try_from(PAGE + 2)?)).with(|heads| {
        (heads.cursor(), heads.get(&Head::<Program>::new("trim")), heads.get(&Head::<Program>::new("f000")))
    })?;
    assert_eq!(first.0, Seq(u64::try_from(PAGE + 1)?));
    assert_eq!(first.1, Some(trim));
    assert_eq!(first.2, Some(filler));
    assert_eq!(second.0, Seq(u64::try_from(PAGE + 2)?));
    assert_eq!(second.1, Some(later));
    assert_eq!(second.2, Some(filler));
    Ok(())
}
