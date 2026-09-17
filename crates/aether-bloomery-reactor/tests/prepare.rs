//! Pure preparation: retained owner, inferred params, poison, and decline.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_bloomery_reactor::{And, Arg, Guard, GuardArg, Owner, PrepareError, ViewArg, prepare};
use aether_bloomery_view::{Heads, View};
use aether_data::{Kind, Storage, StorageData};

const CURRENT: Head<Program> = Head::new("current");
const SOURCE: Head<Tree> = Head::new("source");

struct CurrentCompilation {
    program: Ref<Program>,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)? })
    }
}

struct SameHeads;

impl Guard<HeadMoved<Tree>> for SameHeads {
    type Views = And<Heads, Heads>;

    fn resolve(_trigger: &HeadMoved<Tree>, (left, right): (&Heads, &Heads)) -> Option<Self> {
        assert!(ptr::eq(left, right), "shared Heads must fold once");
        Some(Self)
    }
}

static CONSTRUCTS: AtomicU32 = AtomicU32::new(0);

struct Probe {
    cursor: Seq,
}

impl View for Probe {
    type Error = Infallible;

    fn empty() -> Self {
        CONSTRUCTS.fetch_add(1, Ordering::Relaxed);
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        if let Some(first) = entries.first() {
            assert_eq!(first.seq.0, self.cursor.0 + 1, "catch-up must not replay an already-folded entry");
        }
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

struct ProbeCursor(Seq);

impl Guard<HeadMoved<Tree>> for ProbeCursor {
    type Views = And<Probe, Probe>;

    fn resolve(_trigger: &HeadMoved<Tree>, (left, right): (&Probe, &Probe)) -> Option<Self> {
        assert!(ptr::eq(left, right), "shared Probe must fold once");
        Some(Self(left.cursor()))
    }
}

struct Stuck {
    cursor: Seq,
}

impl View for Stuck {
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

#[derive(Debug)]
struct NeedsStuck;

impl Guard<HeadMoved<Tree>> for NeedsStuck {
    type Views = Stuck;

    fn resolve(_trigger: &HeadMoved<Tree>, _stuck: &Stuck) -> Option<Self> {
        Some(Self)
    }
}

struct NonzeroEmpty;

impl View for NonzeroEmpty {
    type Error = Infallible;

    fn empty() -> Self {
        Self
    }

    fn cursor(&self) -> Seq {
        Seq(1)
    }

    fn advance(&mut self, _entries: &[Entry]) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug)]
struct NeedsNonzero;

impl Guard<HeadMoved<Tree>> for NeedsNonzero {
    type Views = NonzeroEmpty;

    fn resolve(_trigger: &HeadMoved<Tree>, _view: &NonzeroEmpty) -> Option<Self> {
        Some(Self)
    }
}

#[derive(Debug)]
struct Boom;

impl fmt::Display for Boom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("boom")
    }
}

impl Error for Boom {}

static ADVANCES: AtomicU32 = AtomicU32::new(0);

struct Exploding {
    cursor: Seq,
}

impl View for Exploding {
    type Error = Boom;

    fn empty() -> Self {
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        ADVANCES.fetch_add(1, Ordering::Relaxed);
        self.cursor = entries.last().expect("nonempty fold").seq;
        Err(Boom)
    }
}

#[derive(Debug)]
struct NeedsBoom;

impl Guard<HeadMoved<Tree>> for NeedsBoom {
    type Views = Exploding;

    fn resolve(_trigger: &HeadMoved<Tree>, _view: &Exploding) -> Option<Self> {
        Some(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.reactor.note")]
struct Note {
    n: u64,
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Head::<K>::new(name).move_to(to))
}

#[test]
fn direct_heads_and_named_current_guard_stay_at_the_trigger_prefix() -> Result<(), Box<dyn Error>> {
    // Bug: later source moves mutate prepared Heads, or the named guard reads a
    // different prefix than the direct view.
    let program = digest_ref::<Program>(1);
    let first_tree = digest_ref::<Tree>(2);
    let later_tree = digest_ref::<Tree>(3);
    let prefix = [moved(1, "current", program)?, moved(2, "source", first_tree)?];

    let mut owner = Owner::new();
    owner.push(&prefix)?;
    let prepared = owner.prepare_pair::<HeadMoved<Tree>, Heads, CurrentCompilation>()?.expect("current head is bound");
    assert_eq!(prepared.trigger().head(), &SOURCE);
    assert_eq!(prepared.trigger().to(), first_tree);
    assert_eq!(prepared.direct().cursor(), Seq(2));
    assert_eq!(prepared.direct().get(&CURRENT), Some(program));
    assert_eq!(prepared.direct().get(&SOURCE), Some(first_tree));
    assert_eq!(prepared.guard().program, program);

    owner.push(&[moved(3, "source", later_tree)?])?;
    let later = owner.prepare_pair::<HeadMoved<Tree>, Heads, CurrentCompilation>()?.expect("still bound");
    assert_eq!(later.direct().cursor(), Seq(3));
    assert_eq!(later.direct().get(&SOURCE), Some(later_tree));
    assert_eq!(prepared.direct().cursor(), Seq(2));
    assert_eq!(prepared.direct().get(&SOURCE), Some(first_tree));
    Ok(())
}

#[test]
fn one_shot_helper_still_prepares_a_pair() -> Result<(), Box<dyn Error>> {
    // Bug: the retained owner replaces the pair helper instead of wrapping it.
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let prepared = prepare::<HeadMoved<Tree>, Heads, CurrentCompilation>(&[
        moved(1, "current", program)?,
        moved(2, "source", tree)?,
    ])?
    .expect("current head is bound");
    assert_eq!(prepared.direct().cursor(), Seq(2));
    assert_eq!(prepared.guard().program, program);
    Ok(())
}

#[test]
fn missing_named_head_declines_without_prepared_work() -> Result<(), Box<dyn Error>> {
    // Bug: a missing current head becomes an error or a half-built Prepared.
    let tree = digest_ref::<Tree>(1);
    let prepared = prepare::<HeadMoved<Tree>, Heads, CurrentCompilation>(&[moved(1, "source", tree)?])?;
    assert!(prepared.is_none());
    Ok(())
}

#[test]
fn shared_views_fold_once_across_params_and_later_prepares() -> Result<(), Box<dyn Error>> {
    // Bug: Heads/Probe are constructed once per mention or rebuilt from Seq(1)
    // on the next prepare.
    CONSTRUCTS.store(0, Ordering::Relaxed);
    let tree = digest_ref::<Tree>(1);
    let later = digest_ref::<Tree>(2);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "source", tree)?])?;
    let first = owner
        .prepare::<HeadMoved<Tree>, ViewArg<Heads, GuardArg<SameHeads, GuardArg<ProbeCursor>>>>()?
        .expect("guards accept");
    let (_, (heads, (SameHeads, (ProbeCursor(cursor), ())))) = first;
    assert_eq!(heads.cursor(), Seq(1));
    assert_eq!(cursor, Seq(1));
    assert_eq!(CONSTRUCTS.load(Ordering::Relaxed), 1);

    owner.push(&[moved(2, "source", later)?])?;
    let second = owner.prepare::<HeadMoved<Tree>, ViewArg<Heads, GuardArg<ProbeCursor>>>()?.expect("still accepts");
    let (_, (_, (ProbeCursor(cursor), ()))) = second;
    assert_eq!(cursor, Seq(2));
    assert_eq!(heads.cursor(), Seq(1));
    assert_eq!(CONSTRUCTS.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn mixed_parameter_roles_are_inferred_from_types() -> Result<(), Box<dyn Error>> {
    // Bug: only the pair positions (direct, then guard) can be prepared, so a
    // macro cannot emit an arbitrary heads/current/extra list.
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "current", program)?, moved(2, "source", tree)?])?;
    let (trigger, (current, (heads, (SameHeads, ())))) = owner
        .prepare::<HeadMoved<Tree>, Arg<_, CurrentCompilation, Arg<_, Heads, Arg<_, SameHeads>>>>()?
        .expect("roles resolve");
    assert_eq!(trigger.head(), &SOURCE);
    assert_eq!(current.program, program);
    assert_eq!(heads.get(&SOURCE), Some(tree));
    Ok(())
}

#[test]
fn gapped_prefix_is_refused_before_fold() -> Result<(), Box<dyn Error>> {
    // Bug: a hole in the supplied prefix is fed to View::advance.
    let mut owner = Owner::new();
    let error = owner
        .push(&[moved(1, "source", digest_ref::<Tree>(1))?, moved(3, "source", digest_ref::<Tree>(2))?])
        .expect_err("gap");
    assert!(
        matches!(error, PrepareError::Gap { expected, actual } if expected == Seq(2) && actual == Seq(3)),
        "{error}"
    );
    Ok(())
}

#[test]
fn wrong_trigger_kind_is_an_error() -> Result<(), Box<dyn Error>> {
    // Bug: a last entry of the wrong kind is treated as a guard decline.
    let mut owner = Owner::new();
    owner.push(&[entry_for(1, &Note { n: 1 })?])?;
    let error = owner.prepare::<HeadMoved<Tree>, ViewArg<Heads>>().expect_err("kind mismatch");
    assert!(matches!(error, PrepareError::Trigger(_)), "{error}");
    Ok(())
}

#[test]
fn incorrect_fold_cursor_and_advance_errors_fail_closed() -> Result<(), Box<dyn Error>> {
    // Bug: a view that does not consume the prefix still yields Prepared, or a
    // fold Err is turned into a declined guard.
    let entries = [moved(1, "source", digest_ref::<Tree>(1))?];
    let mut owner = Owner::new();
    owner.push(&entries)?;
    let cursor = owner.prepare::<HeadMoved<Tree>, GuardArg<NeedsStuck>>().expect_err("cursor contract");
    assert!(
        matches!(cursor, PrepareError::CursorContract { expected, actual, .. } if expected == Seq(1) && actual == Seq(0)),
        "{cursor}"
    );

    let mut empty_owner = Owner::new();
    empty_owner.push(&entries)?;
    let empty = empty_owner.prepare::<HeadMoved<Tree>, GuardArg<NeedsNonzero>>().expect_err("nonzero empty");
    assert!(matches!(empty, PrepareError::NonzeroEmpty { cursor, .. } if cursor == Seq(1)), "{empty}");
    let poisoned = empty_owner.prepare::<HeadMoved<Tree>, GuardArg<NeedsNonzero>>().expect_err("stays poisoned");
    assert!(matches!(poisoned, PrepareError::Poisoned { .. }), "{poisoned}");
    Ok(())
}

#[test]
fn poisoned_owner_cannot_serve_the_failed_view() -> Result<(), Box<dyn Error>> {
    // Bug: a failed fold is retried, or later prepare ignores the poison flag.
    ADVANCES.store(0, Ordering::Relaxed);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "source", digest_ref::<Tree>(1))?])?;
    let boom = owner.prepare::<HeadMoved<Tree>, GuardArg<NeedsBoom>>().expect_err("advance");
    assert!(matches!(boom, PrepareError::Advance { .. }), "{boom}");
    assert!(owner.is_poisoned());
    assert_eq!(ADVANCES.load(Ordering::Relaxed), 1);

    let again = owner.prepare::<HeadMoved<Tree>, GuardArg<NeedsBoom>>().expect_err("poisoned");
    assert!(
        matches!(again, PrepareError::Poisoned { last_trusted_cursor, .. } if last_trusted_cursor == Seq(0)),
        "{again}"
    );
    assert_eq!(ADVANCES.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn empty_input_has_no_trigger() {
    // Bug: an empty slice prepares at Seq(0) instead of refusing.
    let error = prepare::<HeadMoved<Tree>, Heads, ()>(&[]).expect_err("empty");
    assert!(matches!(error, PrepareError::Empty), "{error}");
}
