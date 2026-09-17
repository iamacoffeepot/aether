//! Lifecycle observations preserve the distinction between attempted and active bytes.

use std::error::Error;

use aether_bloomery_kinds::{
    Detail, Digest, Head, KERNEL_HEAD, OpaqueBytes, REACTORS_HEAD, ReactorLifecycleOutcome, ReactorSet, Ref,
};
use aether_data::{Citation, Citations, Cites, Kind, Storage, StorageData};

fn artifact(byte: u8) -> Ref<OpaqueBytes> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

#[test]
fn conventional_heads_are_typed_and_distinct() {
    assert_eq!(KERNEL_HEAD, Head::<OpaqueBytes>::new("core.kernel"));
    assert_eq!(REACTORS_HEAD, Head::<ReactorSet>::new("core.reactors"));
}

#[test]
fn outcomes_round_trip_and_cite_only_the_relevant_artifact() -> Result<(), Box<dyn Error>> {
    let active = artifact(1);
    let attempted = artifact(2);
    let cases = [
        (ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: active }, Some(active)),
        (
            ReactorLifecycleOutcome::Rejected {
                cluster: KERNEL_HEAD,
                attempted,
                reason: Detail::new("invalid export"),
            },
            Some(attempted),
        ),
        (ReactorLifecycleOutcome::Retired { cluster: KERNEL_HEAD }, None),
    ];

    for (outcome, cited) in cases {
        let bytes = ReactorLifecycleOutcome::encode_storage(&StorageData::from_value(outcome.clone()))?;
        assert_eq!(ReactorLifecycleOutcome::decode_storage(&bytes)?.value, outcome);

        let mut citations = Citations::default();
        outcome.cites(&mut citations);
        let expected = cited.map_or_else(Vec::new, |reference| {
            vec![Citation { kind: OpaqueBytes::ID, bytes: reference.digest().as_bytes().to_vec() }]
        });
        assert_eq!(citations.into_vec(), expected);
    }

    assert_ne!(
        ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: active },
        ReactorLifecycleOutcome::Rejected { cluster: KERNEL_HEAD, attempted: active, reason: Detail::new("refused") }
    );
    Ok(())
}

#[test]
fn equal_artifacts_do_not_merge_cluster_identity() -> Result<(), Box<dyn Error>> {
    let shared = artifact(3);
    let other = Head::<OpaqueBytes>::new("worker");
    let kernel = ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: shared };
    let worker = ReactorLifecycleOutcome::Activated { cluster: other, artifact: shared };
    assert_ne!(kernel, worker);

    for outcome in [kernel, worker] {
        let bytes = ReactorLifecycleOutcome::encode_storage(&StorageData::from_value(outcome.clone()))?;
        assert_eq!(ReactorLifecycleOutcome::decode_storage(&bytes)?.value, outcome);
    }
    Ok(())
}
