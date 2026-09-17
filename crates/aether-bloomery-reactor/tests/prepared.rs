//! Prepared-prefix evaluation uses mailed view snapshots, not a truncated fold.

use std::convert::Infallible;
use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_bloomery_reactor::{
    BundledView, Guard, Output, Owner, PrepareError, PreparedPrefix, PublishedView, Reactor, reactor, snapshot_reactor,
};
use aether_bloomery_view::{Heads, Publish, PublishError, View};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_data::{Kind, Storage, StorageData};

const CURRENT: Head<Program> = Head::new("current");

#[derive(Clone, Debug)]
struct Tally {
    cursor: Seq,
    advances: u32,
}

#[derive(Clone, Debug, aether_data::Schema, serde::Serialize, serde::Deserialize)]
struct TallyWire {
    cursor: u64,
    advances: u32,
}

impl View for Tally {
    type Error = Infallible;

    fn empty() -> Self {
        Self { cursor: Seq(0), advances: 0 }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        self.advances = self.advances.saturating_add(1);
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

impl Publish for Tally {
    fn snapshot(&self) -> Self {
        self.clone()
    }

    fn encode(&self) -> Result<Vec<u8>, PublishError> {
        Ok(encode_to_vec(&TallyWire { cursor: self.cursor.0, advances: self.advances })?)
    }

    fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let wire = decode_from_slice::<TallyWire>(bytes)?;
        Ok(Self { cursor: Seq(wire.cursor), advances: wire.advances })
    }
}

impl BundledView for Tally {
    const NAME: &'static str = "test.bloomery.reactor.tally";
}

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
    advances: u32,
}

impl Output for Publication {}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>, current: CurrentCompilation, heads: Heads, tally: Tally) -> Publication {
        assert_eq!(heads.get(&CURRENT), Some(current.program));
        assert_eq!(*change.to().digest().as_bytes(), [2; 32]);
        Publication { marker: 1, advances: tally.advances }
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

fn owner_with_current_and_source() -> Result<(Owner, Entry), Box<dyn Error>> {
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let current = moved(1, "current", program)?;
    let source = moved(2, "source", tree)?;
    let mut owner = Owner::new();
    owner.push(&[current, source.clone()])?;
    aether_bloomery_reactor::warm_reactor::<SourcePublisher>(&mut owner)?;
    Ok((owner, source))
}

#[test]
fn prepared_prefix_installs_every_snapshot_without_refolding() -> Result<(), Box<dyn Error>> {
    // Bug: a peer given only the trigger entry re-folds views from that truncated
    // prefix, dropping prior heads and constructing a second Tally.
    let (owner, source) = owner_with_current_and_source()?;
    let views = snapshot_reactor::<SourcePublisher>(&owner)?;
    assert!(views.iter().any(|view| view.name == <Heads as Kind>::NAME));
    assert!(views.iter().any(|view| view.name == Tally::NAME));

    let mut peer = PreparedPrefix::from_parts(&source, views).into_owner::<SourcePublisher>()?;
    let intents = SourcePublisher.evaluate(&mut peer)?;
    assert_eq!(intents.len(), 1);
    let publication = intents[0].decode::<Publication>().expect("guarded");
    assert_eq!(publication.marker, 1);
    assert_eq!(publication.advances, 1);
    Ok(())
}

#[test]
fn missing_required_snapshot_is_an_error() -> Result<(), Box<dyn Error>> {
    let (_owner, source) = owner_with_current_and_source()?;
    let error = PreparedPrefix::from_parts(&source, Vec::new())
        .into_owner::<SourcePublisher>()
        .err()
        .expect("missing snapshots");
    assert!(matches!(error, PrepareError::MissingSnapshot { .. }), "{error}");
    Ok(())
}

#[test]
fn duplicate_snapshot_names_are_rejected() -> Result<(), Box<dyn Error>> {
    let (owner, source) = owner_with_current_and_source()?;
    let mut views = snapshot_reactor::<SourcePublisher>(&owner)?;
    views.push(views[0].clone());
    let error =
        PreparedPrefix::from_parts(&source, views).into_owner::<SourcePublisher>().err().expect("duplicate snapshots");
    assert!(matches!(error, PrepareError::DuplicateSnapshot { .. }), "{error}");
    Ok(())
}

#[test]
fn wrong_prefix_snapshot_is_an_error() -> Result<(), Box<dyn Error>> {
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let current = moved(1, "current", program)?;
    let source = moved(2, "source", tree)?;
    let mut early = Owner::new();
    early.push(&[current])?;
    aether_bloomery_reactor::warm_reactor::<SourcePublisher>(&mut early)?;
    let early_views = snapshot_reactor::<SourcePublisher>(&early)?;

    let error = PreparedPrefix::from_parts(&source, early_views)
        .into_owner::<SourcePublisher>()
        .err()
        .expect("wrong-prefix snapshots");
    assert!(matches!(error, PrepareError::CursorContract { .. }), "{error}");
    Ok(())
}

#[test]
fn malformed_snapshot_bytes_are_rejected() -> Result<(), Box<dyn Error>> {
    let (_owner, source) = owner_with_current_and_source()?;
    let views = vec![
        PublishedView { name: String::from(<Heads as Kind>::NAME), bytes: vec![0xff, 0x00, 0x01] },
        PublishedView { name: String::from(Tally::NAME), bytes: vec![0xff, 0x00, 0x01] },
    ];
    let error =
        PreparedPrefix::from_parts(&source, views).into_owner::<SourcePublisher>().err().expect("malformed snapshots");
    assert!(matches!(error, PrepareError::Snapshot { .. }), "{error}");
    Ok(())
}
