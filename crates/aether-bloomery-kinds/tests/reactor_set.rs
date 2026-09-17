//! Reactor-set storage preserves canonical head identity.

use std::error::Error;

use aether_bloomery_kinds::{Head, OpaqueBytes, ReactorSet, ReactorSetError};
use aether_data::{Storage, StorageData, StorageError};

const KERNEL: Head<OpaqueBytes> = Head::new("core.kernel");
const WORKER: Head<OpaqueBytes> = Head::new("worker");

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.reactor_set")]
struct UncheckedSet {
    clusters: Vec<Head<OpaqueBytes>>,
}

#[test]
fn canonical_sets_round_trip_including_empty() -> Result<(), Box<dyn Error>> {
    let empty = ReactorSet::new(Vec::new())?;
    let members = ReactorSet::new(vec![KERNEL, WORKER])?;
    for set in [empty, members] {
        let bytes = ReactorSet::encode_storage(&StorageData::from_value(set.clone()))?;
        assert_eq!(ReactorSet::decode_storage(&bytes)?.value, set);
    }
    Ok(())
}

#[test]
fn constructor_and_storage_decode_refuse_noncanonical_members() -> Result<(), Box<dyn Error>> {
    for (members, reason) in
        [(vec![KERNEL, KERNEL], ReactorSetError::Duplicate), (vec![WORKER, KERNEL], ReactorSetError::Unsorted)]
    {
        assert_eq!(ReactorSet::new(members.clone()), Err(reason));
        let bytes = UncheckedSet::encode_storage(&StorageData::from_value(UncheckedSet { clusters: members }))?;
        match ReactorSet::decode_storage(&bytes) {
            Err(StorageError::Invariant { kind: "Clusters", reason: actual }) => {
                assert_eq!(actual, aether_data::Invariant::reason(&reason));
            }
            other => panic!("expected canonical membership refusal, got {other:?}"),
        }
    }
    Ok(())
}
