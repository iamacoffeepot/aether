//! Typed lifecycle observations use the owning journal's fence and citation checks.

mod common;

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_journal_actor::JournalActor;
use aether_bloomery_kinds::{
    Detail, KERNEL_HEAD, OpaqueBytes, ReactorLifecycleOutcome, RecordReactorLifecycle, RecordReactorLifecycleResult,
    Ref, Utf8Text,
};
use aether_data::Kind;
use aether_substrate::Subname;
use aether_substrate::testing::{bare_substrate, boot_test_chassis_with};

use common::{TestAnchor, caller, reply, request};

#[test]
fn all_outcomes_commit_one_typed_entry_with_cause_and_cited_bytes() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let path = temp.path().join("lifecycle.sqlite");
    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.lifecycle_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let owner = chassis
        .spawn_actor::<JournalActor>(Subname::Named("lifecycle"), path.clone(), ())
        .finish()
        .expect("owner birth");

    let active_bytes = b"active component".to_vec();
    let active = ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: Ref::of_bytes(&active_bytes) };
    let activate = RecordReactorLifecycle {
        expect_head: 0,
        cause: None,
        event: active.clone(),
        artifact_bytes: Some(active_bytes.clone()),
    };
    assert_eq!(
        RecordReactorLifecycle::decode_from_bytes(&activate.encode_into_bytes()).expect("request round trip"),
        activate
    );
    request(&registry, owner, caller_id, 11, &activate);
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 11), RecordReactorLifecycleResult::Committed { seq: 1 });

    let attempted_bytes = b"invalid component".to_vec();
    let rejected = ReactorLifecycleOutcome::Rejected {
        cluster: KERNEL_HEAD,
        attempted: Ref::of_bytes(&attempted_bytes),
        reason: Detail::new("invalid export"),
    };
    request(
        &registry,
        owner,
        caller_id,
        12,
        &RecordReactorLifecycle {
            expect_head: 1,
            cause: Some(1),
            event: rejected.clone(),
            artifact_bytes: Some(attempted_bytes.clone()),
        },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 12), RecordReactorLifecycleResult::Committed { seq: 2 });

    let retired = ReactorLifecycleOutcome::Retired { cluster: KERNEL_HEAD };
    request(
        &registry,
        owner,
        caller_id,
        13,
        &RecordReactorLifecycle { expect_head: 2, cause: Some(2), event: retired.clone(), artifact_bytes: None },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 13), RecordReactorLifecycleResult::Committed { seq: 3 });

    let journal = Journal::open(&path).expect("inspect journal");
    let entries = journal.read(Seq(0), 10).expect("read observations");
    assert_eq!(journal.head().expect("head"), Seq(3));
    assert_eq!(entries.len(), 3);
    for (index, (entry, expected)) in entries.iter().zip([active, rejected, retired]).enumerate() {
        assert_eq!(entry.seq, Seq(index as u64 + 1));
        assert_eq!(entry.kind, ReactorLifecycleOutcome::ID);
        assert_eq!(entry.cause, (index > 0).then_some(Seq(index as u64)));
        assert_eq!(entry.decode::<ReactorLifecycleOutcome>().expect("decode outcome"), expected);
    }
    assert_eq!(
        journal.get_bytes(&Ref::<OpaqueBytes>::of_bytes(&active_bytes).digest()).expect("active artifact"),
        Some((OpaqueBytes::ID, active_bytes))
    );
    assert_eq!(
        journal.get_bytes(&Ref::<OpaqueBytes>::of_bytes(&attempted_bytes).digest()).expect("attempted artifact"),
        Some((OpaqueBytes::ID, attempted_bytes))
    );
}

#[test]
fn refused_observations_leave_events_and_staged_bytes_unchanged() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let path = temp.path().join("refusals.sqlite");
    let mut journal = Journal::open(&path).expect("seed artifact store");
    let mut batch = Batch::new();
    let stored = batch.stage_bytes(b"already stored");
    let wrong_kind = batch.stage_text("text artifact");
    journal.append(Seq(0), &batch).expect("seed artifacts without event");
    drop(journal);

    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.refusal_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let owner = chassis
        .spawn_actor::<JournalActor>(Subname::Named("refusals"), path.clone(), ())
        .finish()
        .expect("owner birth");

    let mismatch_bytes = b"wrong bytes".to_vec();
    request(
        &registry,
        owner,
        caller_id,
        21,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Activated {
                cluster: KERNEL_HEAD,
                artifact: Ref::of_bytes(b"expected bytes"),
            },
            artifact_bytes: Some(mismatch_bytes.clone()),
        },
    );
    assert!(matches!(
        reply::<RecordReactorLifecycleResult>(&rx, 21),
        RecordReactorLifecycleResult::Err { message } if message.contains("do not match")
    ));

    request(
        &registry,
        owner,
        caller_id,
        22,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Retired { cluster: KERNEL_HEAD },
            artifact_bytes: Some(b"not cited".to_vec()),
        },
    );
    assert!(matches!(
        reply::<RecordReactorLifecycleResult>(&rx, 22),
        RecordReactorLifecycleResult::Err { message } if message.contains("retirement")
    ));

    let missing = Ref::<OpaqueBytes>::of_bytes(b"missing artifact");
    request(
        &registry,
        owner,
        caller_id,
        23,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Rejected {
                cluster: KERNEL_HEAD,
                attempted: missing,
                reason: Detail::new("failed before artifact upload"),
            },
            artifact_bytes: None,
        },
    );
    assert!(matches!(
        reply::<RecordReactorLifecycleResult>(&rx, 23),
        RecordReactorLifecycleResult::Err { message } if message.contains("dangling ref")
    ));

    request(
        &registry,
        owner,
        caller_id,
        24,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Activated {
                cluster: KERNEL_HEAD,
                artifact: Ref::from_digest(wrong_kind.digest()),
            },
            artifact_bytes: None,
        },
    );
    assert!(matches!(
        reply::<RecordReactorLifecycleResult>(&rx, 24),
        RecordReactorLifecycleResult::Err { message } if message.contains("prefix mismatch")
    ));

    let existing = ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: stored };
    request(
        &registry,
        owner,
        caller_id,
        25,
        &RecordReactorLifecycle { expect_head: 0, cause: None, event: existing.clone(), artifact_bytes: None },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 25), RecordReactorLifecycleResult::Committed { seq: 1 });

    let stale_bytes = b"stale bytes".to_vec();
    request(
        &registry,
        owner,
        caller_id,
        26,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: Some(1),
            event: ReactorLifecycleOutcome::Rejected {
                cluster: KERNEL_HEAD,
                attempted: Ref::of_bytes(&stale_bytes),
                reason: Detail::new("stale attempt"),
            },
            artifact_bytes: Some(stale_bytes.clone()),
        },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 26), RecordReactorLifecycleResult::HeadMoved { actual: 1 });

    let journal = Journal::open(&path).expect("inspect journal");
    assert_eq!(journal.head().expect("head"), Seq(1));
    let entries = journal.read(Seq(0), 10).expect("read entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decode::<ReactorLifecycleOutcome>().expect("decode only entry"), existing);
    for absent in [
        Ref::<OpaqueBytes>::of_bytes(&mismatch_bytes).digest(),
        Ref::<OpaqueBytes>::of_bytes(b"not cited").digest(),
        missing.digest(),
        Ref::<OpaqueBytes>::of_bytes(&stale_bytes).digest(),
    ] {
        assert_eq!(journal.get_bytes(&absent).expect("absent blob"), None);
    }
    assert_eq!(
        journal.get_bytes(&wrong_kind.digest()).expect("wrong kind remains"),
        Some((Utf8Text::ID, b"text artifact".to_vec()))
    );
    assert_eq!(
        journal.get_bytes(&stored.digest()).expect("preexisting artifact remains"),
        Some((OpaqueBytes::ID, b"already stored".to_vec()))
    );
}

#[test]
fn named_owners_keep_lifecycle_logs_and_artifacts_isolated() {
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let alpha_path = temp.path().join("alpha.sqlite");
    let beta_path = temp.path().join("beta.sqlite");
    let (registry, mailer) = bare_substrate();
    let (caller_id, rx) = caller(&registry, "test.journal_actor.isolation_caller");
    let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
    let alpha = chassis
        .spawn_actor::<JournalActor>(Subname::Named("alpha"), alpha_path.clone(), ())
        .finish()
        .expect("alpha birth");
    let beta = chassis
        .spawn_actor::<JournalActor>(Subname::Named("beta"), beta_path.clone(), ())
        .finish()
        .expect("beta birth");
    assert_ne!(alpha, beta);

    let alpha_bytes = b"alpha component".to_vec();
    request(
        &registry,
        alpha,
        caller_id,
        31,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Activated { cluster: KERNEL_HEAD, artifact: Ref::of_bytes(&alpha_bytes) },
            artifact_bytes: Some(alpha_bytes.clone()),
        },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 31), RecordReactorLifecycleResult::Committed { seq: 1 });

    request(
        &registry,
        beta,
        caller_id,
        32,
        &RecordReactorLifecycle {
            expect_head: 0,
            cause: None,
            event: ReactorLifecycleOutcome::Retired { cluster: KERNEL_HEAD },
            artifact_bytes: None,
        },
    );
    assert_eq!(reply::<RecordReactorLifecycleResult>(&rx, 32), RecordReactorLifecycleResult::Committed { seq: 1 });

    let alpha_journal = Journal::open(&alpha_path).expect("inspect alpha");
    let beta_journal = Journal::open(&beta_path).expect("inspect beta");
    assert_eq!(alpha_journal.head().expect("alpha head"), Seq(1));
    assert_eq!(beta_journal.head().expect("beta head"), Seq(1));
    assert!(matches!(
        alpha_journal.read(Seq(0), 1).expect("alpha entry")[0].decode::<ReactorLifecycleOutcome>(),
        Ok(ReactorLifecycleOutcome::Activated { .. })
    ));
    assert_eq!(
        beta_journal.read(Seq(0), 1).expect("beta entry")[0].decode::<ReactorLifecycleOutcome>().expect("beta outcome"),
        ReactorLifecycleOutcome::Retired { cluster: KERNEL_HEAD }
    );
    assert_eq!(
        alpha_journal.get_bytes(&Ref::<OpaqueBytes>::of_bytes(&alpha_bytes).digest()).expect("alpha blob"),
        Some((OpaqueBytes::ID, alpha_bytes.clone()))
    );
    assert_eq!(
        beta_journal.get_bytes(&Ref::<OpaqueBytes>::of_bytes(&alpha_bytes).digest()).expect("no alpha blob in beta"),
        None
    );
}
