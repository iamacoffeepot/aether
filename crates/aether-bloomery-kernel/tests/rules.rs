//! Kernel rules respond to typed configuration moves at the folded event cursor.

use std::error::Error;

use aether_bloomery_kernel::{KernelPolicy, KernelReconcileIntent};
use aether_bloomery_kinds::{
    Digest, Entry, Head, HeadMoved, KERNEL_HEAD, OpaqueBytes, REACTORS_HEAD, ReactorSet, Ref, Seq, Tree,
};
use aether_bloomery_reactor::Owner;
use aether_data::{Kind, Storage, StorageData};

fn reference<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn moved<K: Kind + 'static>(seq: u64, head: Head<K>, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: HeadMoved::<K>::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<K>::encode_storage(&StorageData::from_value(head.move_to(to)))?,
    })
}

fn evaluate(owner: &mut Owner, entry: Entry) -> Result<Vec<KernelReconcileIntent>, Box<dyn Error>> {
    owner.push(&[entry])?;
    Ok(owner
        .evaluate(&KernelPolicy)?
        .iter()
        .map(|intent| intent.decode::<KernelReconcileIntent>().expect("kernel intent"))
        .collect())
}

#[test]
fn only_conventional_set_root_and_byte_head_moves_reconcile() -> Result<(), Box<dyn Error>> {
    let mut owner = Owner::new();
    assert!(evaluate(&mut owner, moved(1, Head::<ReactorSet>::new("other.reactors"), reference(1))?)?.is_empty());
    assert_eq!(
        evaluate(&mut owner, moved(2, REACTORS_HEAD, reference(2))?)?,
        vec![KernelReconcileIntent { event_seq: 2 }]
    );
    assert_eq!(
        evaluate(&mut owner, moved(3, KERNEL_HEAD, reference(3))?)?,
        vec![KernelReconcileIntent { event_seq: 3 }]
    );
    assert_eq!(
        evaluate(&mut owner, moved(4, Head::<OpaqueBytes>::new("worker"), reference(4))?)?,
        vec![KernelReconcileIntent { event_seq: 4 }]
    );
    Ok(())
}

#[test]
fn other_typed_specializations_and_unrelated_events_decline() -> Result<(), Box<dyn Error>> {
    let mut owner = Owner::new();
    assert!(evaluate(&mut owner, moved(1, Head::<Tree>::new("source"), reference(1))?)?.is_empty());
    let unrelated =
        Entry { seq: Seq(2), kind: "test.unrelated".to_owned(), cause: None, recorded_at_millis: 0, bytes: Vec::new() };
    assert!(evaluate(&mut owner, unrelated)?.is_empty());
    assert_eq!(
        evaluate(&mut owner, moved(3, REACTORS_HEAD, reference(3))?)?,
        vec![KernelReconcileIntent { event_seq: 3 }]
    );
    Ok(())
}
