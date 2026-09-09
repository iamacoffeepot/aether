#![cfg(all(unix, feature = "github"))]

//! A candidate that edits an instruction file in its own tree still runs under
//! the sealed bundle (ADR-0214 §The candidate cannot replace the process).
//!
//! The construct lane is scripted to rewrite `construct_instructions.md` in the
//! checkout. That edit is captured as the candidate. The host still hands every
//! model dispatch the authorized bundle's exact bytes, written outside the
//! checkout, and the assembled prompt manifest is retained in the artifact
//! store under the address the dispatch row names.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{CONSTRUCT_IMPLEMENT_COMMAND, Digest, ModelProcessInstructions, REVIEW_CRITIC_COMMAND};
use aether_chassis_bloomery::artifacts::GetResult;
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::{SqliteStore, StoreBackend};
use aether_data::Kind;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots};

#[test]
fn a_candidate_that_edits_instruction_files_still_runs_the_sealed_bundle() {
    let roots = HarnessRoots::create();
    let script = LaneScript::all_passing().then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::EditsInstructions);
    let mut harness = HarnessBuilder::lane(&script).roots(&roots).start("candidate-cannot-replace-the-process");

    let pin = harness.instructions();
    let mut store = SqliteStore::open(&roots.store_path()).expect("the coordinator's journal opens");
    let (kind, bundle_bytes, _) =
        store.lookup_config(pin.as_bytes()).expect("the pin looks up").expect("the authorized bundle is stored");
    assert_eq!(kind, ModelProcessInstructions::NAME);

    let mut retained = Vec::new();
    pump_until(&mut harness, "construct and review both dispatched", |harness| {
        for order in harness.orders() {
            if let Some(address) = order.prompt_manifest {
                if !retained.contains(&address) {
                    retained.push(address);
                }
            }
        }
        let ledger = harness.ledger();
        ledger.iter().any(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND)
            && ledger.iter().any(|run| run.command == REVIEW_CRITIC_COMMAND)
    });

    let ledger = harness.ledger();
    let construct = ledger.iter().find(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).expect("construct dispatched");
    let review = ledger.iter().find(|run| run.command == REVIEW_CRITIC_COMMAND).expect("review dispatched");

    for run in [construct, review] {
        let path = run.instruction_manifest.as_deref().expect("the host named the bundle file in the lane env");
        let handed = fs::read(path).expect("the named file is readable");
        assert_eq!(handed, bundle_bytes, "{} must consume the sealed bundle, not the checkout", run.command);
        assert_eq!(
            run.instruction_manifest_digest.as_deref(),
            Some(pin.to_hex().as_str()),
            "{} records the sealed pin",
            run.command
        );
        assert!(
            run.env.iter().any(|name| name == aether_bloomery::INSTRUCTION_MANIFEST_ENV),
            "{} holds the manifest path env",
            run.command
        );
        assert!(
            run.env.iter().any(|name| name == aether_bloomery::INSTRUCTION_MANIFEST_DIGEST_ENV),
            "{} holds the manifest digest env",
            run.command
        );
    }

    let edited = construct
        .worktree
        .as_deref()
        .map(std::path::Path::new)
        .map(|tree| tree.join("xtask/src/transform/construct_instructions.md"))
        .expect("construct records its checkout");
    assert!(
        fs::read_to_string(&edited).unwrap_or_default().contains("replaced process instructions"),
        "the candidate did edit the instruction file in the checkout: {edited:?}"
    );

    assert!(!retained.is_empty(), "a model dispatch records the assembled prompt-manifest address");
    for address in &retained {
        assert_manifest_artifact(&harness, address);
    }
}

fn assert_manifest_artifact(harness: &aether_harness_bloomery::ScenarioHarness, stored: &[u8]) {
    let digest = Digest::from_slice(stored).expect("the stored address is a digest");
    match harness.artifact(&digest.to_hex()) {
        GetResult::Ok { bytes, .. } => {
            assert!(!bytes.is_empty(), "the artifact store holds the assembled manifest bytes");
            assert_eq!(aether_bloomery::Digest::of_wire_bytes(&bytes), digest, "the row names those bytes' address");
        }
        other => panic!("the dispatch row's prompt-manifest address must resolve in the artifact store, got {other:?}"),
    }
}

fn pump_until(
    harness: &mut aether_harness_bloomery::ScenarioHarness,
    what: &str,
    mut ready: impl FnMut(&mut aether_harness_bloomery::ScenarioHarness) -> bool,
) {
    let deadline = Instant::now() + Duration::from_mins(2);
    while !ready(harness) {
        assert!(Instant::now() < deadline, "{what} did not happen inside the scenario's budget");
        harness.tick();
        thread::sleep(Duration::from_millis(25));
    }
}
