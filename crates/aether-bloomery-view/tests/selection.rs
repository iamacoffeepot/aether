//! Reactor recipients come from one exact pre-event journal prefix.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, OpaqueBytes, ReactorSet, Ref, Seq};
use aether_bloomery_view::{Heads, SelectionError, View, select_reactors};
use aether_data::{Kind, Storage, StorageData};

const SET_ROOT: Head<ReactorSet> = Head::new("core.reactors");
const KERNEL: Head<OpaqueBytes> = Head::new("core.kernel");
const WORKER: Head<OpaqueBytes> = Head::new("worker");

fn reference<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn moved<K: Kind + 'static>(seq: u64, head: &Head<K>, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    let event = head.move_to(to);
    Ok(Entry {
        seq: Seq(seq),
        kind: aether_bloomery_kinds::HeadMoved::<K>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: aether_bloomery_kinds::HeadMoved::<K>::encode_storage(&StorageData::from_value(event))?,
    })
}

#[test]
fn missing_roots_members_and_kernel_are_explicit_errors() -> Result<(), Box<dyn Error>> {
    let set_ref = reference::<ReactorSet>(1);
    let set = ReactorSet::new(vec![KERNEL, WORKER])?;
    let empty = Heads::new();
    assert!(matches!(
        select_reactors(&empty, Seq(1), &SET_ROOT, &KERNEL, set_ref, &set),
        Err(SelectionError::SetUnbound { .. })
    ));
    assert_eq!(
        select_reactors(&empty, Seq(0), &SET_ROOT, &KERNEL, set_ref, &set),
        Err(SelectionError::InvalidEventSeq)
    );

    let mut heads = Heads::new();
    heads.apply(&moved(1, &SET_ROOT, set_ref)?)?;
    assert!(matches!(
        select_reactors(&heads, Seq(2), &SET_ROOT, &KERNEL, reference(2), &set),
        Err(SelectionError::SetMismatch { .. })
    ));
    assert_eq!(
        select_reactors(&heads, Seq(2), &SET_ROOT, &KERNEL, set_ref, &ReactorSet::new(Vec::new())?),
        Err(SelectionError::KernelMissing { head: KERNEL })
    );
    assert_eq!(
        select_reactors(&heads, Seq(2), &SET_ROOT, &KERNEL, set_ref, &set),
        Err(SelectionError::MemberUnbound { head: KERNEL })
    );

    heads.apply(&moved(2, &KERNEL, reference::<OpaqueBytes>(3))?)?;
    assert_eq!(
        select_reactors(&heads, Seq(3), &SET_ROOT, &KERNEL, set_ref, &set),
        Err(SelectionError::MemberUnbound { head: WORKER })
    );
    Ok(())
}

#[test]
fn each_event_uses_its_predecessor_prefix_across_set_and_bundle_moves() -> Result<(), Box<dyn Error>> {
    let a = reference::<OpaqueBytes>(1);
    let b = reference::<OpaqueBytes>(2);
    let c = reference::<OpaqueBytes>(3);
    let both_ref = reference::<ReactorSet>(4);
    let kernel_only_ref = reference::<ReactorSet>(5);
    let both = ReactorSet::new(vec![KERNEL, WORKER])?;
    let kernel_only = ReactorSet::new(vec![KERNEL])?;
    let entries = [
        moved(1, &KERNEL, a)?,
        moved(2, &WORKER, b)?,
        moved(3, &SET_ROOT, both_ref)?,
        moved(4, &KERNEL, c)?,
        moved(5, &WORKER, c)?,
        moved(6, &SET_ROOT, kernel_only_ref)?,
        moved(7, &KERNEL, a)?,
    ];
    let mut heads = Heads::new();
    heads.advance(&entries[..3])?;

    for (event, expected) in
        [(4, vec![(KERNEL, a), (WORKER, b)]), (5, vec![(KERNEL, c), (WORKER, b)]), (6, vec![(KERNEL, c), (WORKER, c)])]
    {
        let actual = select_reactors(&heads, Seq(event), &SET_ROOT, &KERNEL, both_ref, &both)?;
        assert_eq!(
            actual.iter().map(|selected| (selected.head.clone(), selected.artifact)).collect::<Vec<_>>(),
            expected
        );
        heads.apply(&entries[usize::try_from(event - 1)?])?;
    }

    let before_set_change = select_reactors(&heads, Seq(7), &SET_ROOT, &KERNEL, kernel_only_ref, &kernel_only)?;
    assert_eq!(before_set_change.len(), 1);
    assert_eq!(before_set_change[0].head, KERNEL);
    assert_eq!(before_set_change[0].artifact, c);
    heads.apply(&entries[6])?;
    let successor = select_reactors(&heads, Seq(8), &SET_ROOT, &KERNEL, kernel_only_ref, &kernel_only)?;
    assert_eq!(successor[0].artifact, a);
    assert!(matches!(
        select_reactors(&heads, Seq(7), &SET_ROOT, &KERNEL, kernel_only_ref, &kernel_only),
        Err(SelectionError::WrongPrefix { expected: Seq(6), actual: Seq(7) })
    ));
    Ok(())
}
