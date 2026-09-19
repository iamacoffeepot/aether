//! Signature-based evaluation: named guards, shared views, and refutable triggers.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Program, Ref, Seq, SetHead, Tree};
use aether_bloomery_reactor::{ArmVisitor, Guard, Output, Owner, Params, PrepareError, Reactor, Trigger, reactor};
use aether_bloomery_view::Heads;
use aether_data::{Kind, Storage, StorageData};

const CURRENT: Head<Program> = Head::new("current");
const SOURCE: Head<Tree> = Head::new("source");
const PUBLISHED: Head<Tree> = Head::new("published");

struct CurrentCompilation {
    program: Ref<Program>,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)? })
    }
}

struct CurrentSource;

impl Guard<Compilation> for CurrentSource {
    type Views = Heads;

    fn resolve(trigger: &Compilation, heads: &Heads) -> Option<Self> {
        let current = heads.get(&SOURCE)?;
        let source = match trigger {
            Compilation::Succeeded { source, .. } | Compilation::Failed { source } => *source,
        };
        if current.digest().as_bytes() != &source {
            return None;
        }
        Some(Self)
    }
}

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.bloomery.reactor.compilation")]
enum Compilation {
    Succeeded { source: [u8; 32], output: [u8; 32] },
    Failed { source: [u8; 32] },
}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish_source(&self, change: HeadMoved<Tree>, current: CurrentCompilation, heads: Heads) -> SetHead {
        assert_eq!(heads.get(&CURRENT), Some(current.program));
        SetHead::new(&PUBLISHED, None, change.to())
    }

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, heads: Heads) -> SetHead {
        assert!(heads.cursor().0 > 0);
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct CompilationPublisher;

#[reactor]
impl Reactor for CompilationPublisher {
    const NAMESPACE: &'static str = "compilation.publisher";

    #[rule]
    fn publish_compiled(&self, Compilation::Succeeded { output, .. }: Compilation, _current: CurrentSource) -> SetHead {
        SetHead::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes(output)))
    }

    #[rule]
    fn ignore_failed(&self, Compilation::Failed { source }: Compilation) -> SetHead {
        SetHead::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes(source)))
    }
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Head::<K>::new(name).move_to(to))
}

#[test]
fn named_guard_declines_without_body_checks() -> Result<(), Box<dyn Error>> {
    // Bug: a missing current head is checked in the arm body, or becomes an error,
    // or it also suppresses a sibling arm that did not name the guard.
    let tree = digest_ref::<Tree>(1);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "source", tree)?])?;
    let intents = SourcePublisher.evaluate(&mut owner)?;
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].rule(), "note_heads");
    Ok(())
}

#[test]
fn named_guard_and_direct_heads_publish() -> Result<(), Box<dyn Error>> {
    // Bug: the signature guard is not enough and the arm still needs a stale-head
    // conditional, or Heads is a different instance than the guard's view.
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "current", program)?, moved(2, "source", tree)?])?;
    let intents = SourcePublisher.evaluate(&mut owner)?;
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].rule(), "publish_source");
    assert_eq!(intents[1].rule(), "note_heads");
    let first = intents[0].decode::<SetHead>().expect("mail-capable output");
    assert_eq!(first.to(), Digest::from_bytes([2; 32]));
    assert_eq!(intents[0].kind(), SetHead::ID);
    Ok(())
}

#[test]
fn two_arms_share_the_folded_view_across_events() -> Result<(), Box<dyn Error>> {
    // Bug: evaluating two arms rebuilds Heads, or a later event mutates the first
    // evaluation's owned proposal.
    let program = digest_ref::<Program>(1);
    let first_tree = digest_ref::<Tree>(2);
    let later_tree = digest_ref::<Tree>(3);
    let mut owner = Owner::new();
    owner.push(&[moved(1, "current", program)?, moved(2, "source", first_tree)?])?;
    let first = SourcePublisher.evaluate(&mut owner)?;
    assert_eq!(first.len(), 2);
    let first_to = first[0].decode::<SetHead>().expect("first").to();

    owner.push(&[moved(3, "source", later_tree)?])?;
    let second = SourcePublisher.evaluate(&mut owner)?;
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].decode::<SetHead>().expect("later").to(), Digest::from_bytes([3; 32]));
    assert_eq!(first[0].decode::<SetHead>().expect("retained").to(), first_to);
    Ok(())
}

#[test]
fn refutable_trigger_selects_the_matching_arm() -> Result<(), Box<dyn Error>> {
    // Bug: both enum cases run, or a failed match is an error instead of a decline.
    let source = digest_ref::<Tree>(4);
    let mut owner = Owner::new();
    owner.push(&[
        moved(1, "source", source)?,
        entry_for(2, &Compilation::Succeeded { source: [4; 32], output: [9; 32] })?,
    ])?;
    let intents = CompilationPublisher.evaluate(&mut owner)?;
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].rule(), "publish_compiled");
    assert_eq!(intents[0].decode::<SetHead>().expect("success").to(), Digest::from_bytes([9; 32]));

    owner.push(&[entry_for(3, &Compilation::Failed { source: [4; 32] })?])?;
    let failed = CompilationPublisher.evaluate(&mut owner)?;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].rule(), "ignore_failed");
    Ok(())
}

#[test]
fn stale_source_guard_declines_without_body_checks() -> Result<(), Box<dyn Error>> {
    // Bug: a compilation for an older source is published because the arm body
    // compares heads itself, or the guard is treated as suspended work.
    let live = digest_ref::<Tree>(5);
    let mut owner = Owner::new();
    owner.push(&[
        moved(1, "source", live)?,
        entry_for(2, &Compilation::Succeeded { source: [1; 32], output: [8; 32] })?,
    ])?;
    let intents = CompilationPublisher.evaluate(&mut owner)?;
    assert!(intents.is_empty());
    Ok(())
}

#[test]
fn unmatched_stored_kind_declines_rather_than_failing() -> Result<(), Box<dyn Error>> {
    // Bug: a compilation event errors a source-head reactor instead of declining.
    let mut owner = Owner::new();
    owner.push(&[entry_for(1, &Compilation::Failed { source: [1; 32] })?])?;
    let intents = SourcePublisher.evaluate(&mut owner)?;
    assert!(intents.is_empty());
    Ok(())
}

#[test]
fn well_formed_head_moved_specialization_declines_rather_than_failing() -> Result<(), Box<dyn Error>> {
    // Bug: HeadMoved<Program> shares bloomery.head_moved with HeadMoved<Tree>
    // and a source-head reactor treats the payload discriminator as a storage
    // error instead of an unmatched trigger.
    let mut owner = Owner::new();
    owner.push(&[moved(1, "current", digest_ref::<Program>(1))?])?;
    let intents = SourcePublisher.evaluate(&mut owner)?;
    assert!(intents.is_empty());
    assert_eq!(owner.cursor(), Seq(1));
    Ok(())
}

#[test]
fn malformed_matching_head_moved_is_an_error() -> Result<(), Box<dyn Error>> {
    // Bug: a corrupt bloomery.head_moved payload is declined as unmatched.
    let mut owner = Owner::new();
    owner.push(&[Entry {
        seq: Seq(1),
        kind: HeadMoved::<Tree>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: vec![0xff, 0x00],
    }])?;
    let error = SourcePublisher.evaluate(&mut owner).expect_err("malformed payload");
    assert!(!error.is_unknown_trigger(), "{error}");
    assert!(matches!(error, PrepareError::Trigger(_)), "{error}");
    Ok(())
}

#[test]
fn visit_arms_exposes_each_rule() {
    struct Names(Vec<&'static str>);
    impl ArmVisitor for Names {
        fn visit<T, L, O>(&mut self, name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            O: Output,
        {
            self.0.push(name);
        }
    }

    let mut names = Names(Vec::new());
    SourcePublisher::visit_arms(&mut names);
    assert_eq!(names.0, ["publish_source", "note_heads"]);
}

#[test]
fn empty_owner_is_an_error() {
    let error = SourcePublisher.evaluate(&mut Owner::new()).expect_err("empty");
    assert!(matches!(error, PrepareError::Empty), "{error}");
}
