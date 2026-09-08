//! Replay of a verify-failed row naming only the later of two appended
//! identities must charge the same rolls the live fold charged (#5838).

mod common;

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Event, Evidence, EvidenceKind, Fact, Outcome, PipelineManifest, ResolvedConfigs,
    Snapshot, SpendWindow, StageId, VerifyFailure, VerifyFailureSet, VerifyGateSet, decode_recorded_event, reduce,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use common::{digest, event, membership, workpiece};

fn appended_manifest() -> PipelineManifest {
    let mut manifest = PipelineManifest::compiled();
    manifest.verifiers.identities.push(String::from("verify.a"));
    manifest.verifiers.identities.push(String::from("verify.b"));
    manifest
}

fn draft_with_manifest(manifest: &PipelineManifest) -> (BloomDraft, ResolvedConfigs) {
    let mut configs = ConfigRegistry::default();
    configs.insert::<PipelineManifest>(manifest.address());
    let mut resolved = ResolvedConfigs::default();
    resolved.insert(manifest.address(), PipelineManifest::NAME, to_vec(manifest).expect("manifest encodes"), None);
    (BloomDraft { proposals: vec![membership("wp", 10)], base: digest(1), configs, ..BloomDraft::default() }, resolved)
}

fn replay(event: &Event) -> Event {
    decode_recorded_event(&to_vec(event).expect("event encodes"), None).expect("current-shape event decodes")
}

#[test]
fn replaying_appended_verifier_rows_charges_the_same_rolls_as_the_live_fold() {
    // Plausible bug: journal decode interns unknown names by arrival, so a row
    // naming only verify.b (declared at 11) lands on bit 10. Folding that row
    // after one that named verify.a then spends a repair roll the live intern
    // never charged, because both rows occupy the same bit.
    let manifest = appended_manifest();
    let verify_a = manifest.intern("verify.a").expect("verify.a is declared at 10");
    let verify_b = manifest.intern("verify.b").expect("verify.b is declared at 11");
    assert_eq!(verify_a.position(), 10);
    assert_eq!(verify_b.position(), 11);

    let (draft, configs) = draft_with_manifest(&manifest);
    let spec = draft.seal();
    let bloom = spec.id();
    let mut live =
        Snapshot::new(digest(1)).with_green_base_under(digest(1), VerifyGateSet::base_of(&manifest).digest());
    let mut replayed = live.clone();

    let fold = |snapshot: &mut Snapshot, event: &Event| {
        let decisions = reduce(snapshot, event, &configs, &SpendWindow::default());
        *snapshot = snapshot.apply(event, &decisions, &configs);
        decisions
    };

    let seal = event("seal", Fact::Seal(spec));
    fold(&mut live, &seal);
    fold(&mut replayed, &replay(&seal));

    let construct = event(
        "construct",
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Construct,
            passed: true,
            evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(90) },
            candidate: None,
        },
    );
    fold(&mut live, &construct);
    fold(&mut replayed, &replay(&construct));

    let fail = |key: &str, failed: VerifyFailure, detail: u8| {
        event(
            key,
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence {
                    subject: digest(10),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(detail),
                },
                failed_verifiers: VerifyFailureSet::one(failed),
            },
        )
    };

    let live_a = fail("verify-a", verify_a, 80);
    let live_b = fail("verify-b", verify_b, 81);
    let decoded_a = replay(&live_a);
    let decoded_b = replay(&live_b);

    match (&decoded_a.fact, &decoded_b.fact) {
        (
            Fact::VerifyFailed { failed_verifiers: decoded_a_set, .. },
            Fact::VerifyFailed { failed_verifiers: decoded_b_set, .. },
        ) => {
            assert_eq!(decoded_a_set.positions().collect::<Vec<_>>(), [10]);
            assert_eq!(
                decoded_b_set.positions().collect::<Vec<_>>(),
                [10],
                "each independently decoded row interned its unknown name onto the first free declared bit",
            );
        }
        _ => panic!("decoded events must remain VerifyFailed"),
    }

    assert!(matches!(fold(&mut live, &live_a).outcome, Outcome::RefineReentered { rolls: 0, .. }));
    assert!(matches!(fold(&mut replayed, &decoded_a).outcome, Outcome::RefineReentered { rolls: 0, .. }));

    let refine = event(
        "refine",
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Refine,
            passed: true,
            evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(91) },
            candidate: None,
        },
    );
    fold(&mut live, &refine);
    fold(&mut replayed, &replay(&refine));

    assert!(matches!(fold(&mut live, &live_b).outcome, Outcome::RefineReentered { rolls: 0, .. }));
    assert!(
        matches!(fold(&mut replayed, &decoded_b).outcome, Outcome::RefineReentered { rolls: 0, .. }),
        "replay must not spend a roll on a second novel identity interned onto the same arrival bit",
    );

    let live_progress = live.blooms.get(&bloom).expect("sealed").progress.get(&workpiece("wp")).expect("at a stage");
    let replay_progress =
        replayed.blooms.get(&bloom).expect("sealed").progress.get(&workpiece("wp")).expect("at a stage");
    assert_eq!(live_progress.repair_rolls, 0);
    assert_eq!(replay_progress.repair_rolls, live_progress.repair_rolls);
    assert_eq!(
        replay_progress.seen_verify_failures.positions().collect::<Vec<_>>(),
        live_progress.seen_verify_failures.positions().collect::<Vec<_>>(),
    );
    assert_eq!(live_progress.seen_verify_failures.positions().collect::<Vec<_>>(), [10, 11]);
}
