//! Prepared-prefix evaluation uses mailed view snapshots, not a truncated fold.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_bloomery_reactor::{Guard, Output, Owner, PreparedPrefix, Reactor, reactor};
use aether_bloomery_view::Heads;
use aether_data::{Kind, Storage, StorageData};

const CURRENT: Head<Program> = Head::new("current");

struct CurrentCompilation {
    program: Ref<Program>,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)? })
    }
}

#[aether_data::kind(name = "test.bloomery.reactor.prepared_publication", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>, current: CurrentCompilation, heads: Heads) -> Publication {
        assert_eq!(heads.get(&CURRENT), Some(current.program));
        assert_eq!(*change.to().digest().as_bytes(), [2; 32]);
        Publication { marker: 1 }
    }
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
fn prepared_prefix_keeps_prior_heads_without_replaying_them() -> Result<(), Box<dyn Error>> {
    // Bug: a peer given only the trigger entry re-folds Heads from that truncated
    // prefix and drops the current-program binding the views owner already had.
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let current = moved(1, "current", program)?;
    let source = moved(2, "source", tree)?;
    let mut owner = Owner::new();
    owner.push(&[current, source.clone()])?;
    owner.warm::<Heads>()?;
    assert_eq!(owner.published_heads().cursor(), Seq(2));

    let mut peer = PreparedPrefix::from_entry(&source, owner.published_heads()).into_owner()?;
    let intents = SourcePublisher.evaluate(&mut peer)?;
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].decode::<Publication>().expect("guarded").marker, 1);
    Ok(())
}
