//! A construct-family lane that cannot finish inside its declared surface
//! names the paths it needs, and the estate answers: the machinery grants a
//! delta its tier policy resolves at `auto`, and parks the member awaiting a
//! person for anything above it (ADR-0207).
//!
//! The sibling of `a_declining_refine_lane_leaves_a_visible_park`: that one
//! pins the plain, request-less park; this one pins the typed request and both
//! of its answers. The whole lane → executor → intake → reducer → projection →
//! `/view` path is real, which is the only place either answer can be observed
//! at all — the paths cross four crates between the evidence file and the
//! served document.
//!
//! Pre-fix (the park): nothing read the lane's request. A decline reached
//! `/view` as a bare park with no remedy attached, so an operator saw a stopped
//! member and no statement of what would unstop it.
//!
//! Pre-fix (the grant): the grant resolved its tier policy from the bloom's
//! sealed configuration registry alone. A real bloom seals no policy — the
//! file the seal door falls back to is where the tiers live — so the resolve
//! came back empty, and an `auto` delta over an `auto` surface parked for a
//! person the policy had already said it did not need.

#![allow(clippy::unwrap_used)]

use aether_bloomery::testing::digest;
use aether_bloomery::{StageId, digest_of};
use aether_chassis_bloomery::bloomery::mock_lane::REQUESTED_PATH;
use aether_harness_bloomery::{BloomeryHarness, HarnessBuilder, LaneScript, Oracle, Repo};

/// The fallback tier policy the granting scenario's coordinator loads.
///
/// Both the member's declared surface and the delta it asks for resolve `auto`,
/// which is the shape the machinery may answer on its own: `gate_widening`
/// judges the *widened* surface, so a delta over a surface that itself sits
/// above `auto` is a person's call however cheap the addition looks.
const AUTO_TIER_POLICY: &str = r#"default = "judge"

[[rules]]
glob = "crates/example-a/**"
tier = "auto"

[[rules]]
glob = "crates/example-b/**"
tier = "auto"
"#;

#[test]
fn a_declining_lane_requests_the_surface_it_needs() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(
        &aether_bloomery::WorkpieceId("wp".into()),
        StageId::Construct,
        &[LaneScript::DeclineRequestingSurface],
    );
    let bloom = harness.seal_member("wp", digest(0x51));

    harness
        .run_until(|harness| harness.bloom(bloom).members.iter().any(|member| member.awaiting_surface.is_some()), 40);

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    let awaiting = member.awaiting_surface.as_ref().expect("the request reaches the served document");

    // The paths and their reasons are on the document, so an operator reads
    // what to widen without opening an evidence file.
    assert_eq!(awaiting.paths.len(), 1, "the lane asked for exactly what it named: {awaiting:?}");
    assert_eq!(awaiting.paths[0].path, REQUESTED_PATH);
    assert!(!awaiting.paths[0].reason.is_empty(), "a requested path carries the line that justifies it");
    assert_eq!(awaiting.scope_revision, member.scope_revision, "a request is bound to the revision it amends");
    assert_eq!(awaiting.requests, 1);

    // The three park classes stay distinguishable where an operator reads
    // them: a decline *with* a request lights this field and nothing else.
    assert!(member.wedge.is_none(), "asking for surface is not a wedge");
    assert!(member.pending_decision.is_none(), "asking for surface is not an ADR-0151 question");

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));

    // The park holds: the coordinator does not re-run a refusal a second lap
    // would reproduce verbatim, and no attempt burns while it waits.
    let before = harness.bloom(bloom).members[0].cursor.clone();
    for _ in 0..5 {
        harness.tick();
    }
    let after = harness.bloom(bloom).members[0].clone();
    assert_eq!(after.cursor, before, "a parked member spends no attempt and does not move");
    assert!(after.awaiting_surface.is_some(), "the park persists until it is answered");
    assert!(harness.outstanding().is_empty(), "nothing was dispatched against the parked member");
}

#[test]
fn an_auto_tier_request_is_granted_against_the_policy_file_a_bloom_did_not_seal() {
    let repo = Repo::with_example_project();
    let mut harness = HarnessBuilder::local_authority(&repo)
        .hold_repo(repo)
        .file_approval_policy(AUTO_TIER_POLICY)
        .start("bloomery-harness");
    harness.script_lane(
        &aether_bloomery::WorkpieceId("wp".into()),
        StageId::Construct,
        &[LaneScript::DeclineRequestingSurface],
    );

    // A real scope revision, because the grant reads the surface it widens out
    // of the commission store and writes the successor back into it. The bloom
    // seals no `aether.bloomery.approval_policy` of its own, exactly as every
    // bloom the coordinator has ever walked seals none.
    let sealed = harness.author_scope_revision("wp", &["crates/example-a/**"]);
    let bloom = harness.seal_member("wp", sealed);

    // Either answer ends the wait, so what distinguishes them is the assertion
    // below rather than a tick budget running out.
    harness.run_until(
        |harness| {
            let member = &harness.bloom(bloom).members[0];
            member.awaiting_surface.is_some() || member.cursor.as_ref().is_some_and(|cursor| cursor.candidate.is_some())
        },
        60,
    );

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    assert!(
        member.awaiting_surface.is_none(),
        "an auto-tier delta over an auto-tier surface is the machinery's own to grant, not a person's: {member:?}",
    );
    assert!(
        member.cursor.as_ref().is_some_and(|cursor| cursor.candidate.is_some()),
        "the granted member re-entered its construct stage and produced a candidate: {member:?}",
    );

    // The grant is a stored, approved successor revision rather than a flag:
    // the commission and the bloom have to agree about what was authorized, and
    // a pin the commission cannot produce is a pin the seal door refuses.
    let widened = harness
        .scope_revision(sealed)
        .expect("the sealed revision is in the commission store")
        .with_widened_surface(&["crates/example-b/**".to_owned()]);
    let granted = harness
        .scope_revision(digest_of(&widened))
        .expect("the grant wrote the widened successor into the commission store");
    assert_eq!(granted.predecessor, Some(sealed), "the successor chains to the revision the request named");
    assert_eq!(
        granted.declared_surface,
        vec!["crates/example-a/**".to_owned(), "crates/example-b/**".to_owned()],
        "the grant widened the sealed surface by the glob covering the requested path",
    );

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
