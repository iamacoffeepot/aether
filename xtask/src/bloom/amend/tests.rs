//! What the amendment's own code decides, as distinct from what the shared
//! `aether-bloomery` primitives decide (the tier ladder and the surface
//! algebra have their tests there, beside the implementation both this command
//! and the coordinator's seal door stand on).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, process};

use aether_bloomery::{ApprovalPolicy, ApprovalRule, BloomStatus, KeyId, Tier};

use super::revision::OperatorKey;
use super::{TipStanding, request, sibling_is_sealable, siblings, surface, tip_standing};
use crate::bloom::dto::{
    AwaitingSurfaceView, BloomView, CommissionShowView, DigestHex, MemberView, SurfacePathRequest, WithdrawnView,
};

fn digest(seed: u8) -> DigestHex {
    DigestHex::from_bytes([seed; 32])
}

fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("aether-amend-{tag}-{}-{seq}", process::id()));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn member(awaiting: Option<AwaitingSurfaceView>) -> MemberView {
    MemberView {
        workpiece: "example-a".to_owned(),
        scope_revision: digest(1),
        awaiting_surface: awaiting,
        withdrawn: None,
        cursor: None,
    }
}

fn sibling(withdrawn: Option<WithdrawnView>) -> MemberView {
    MemberView {
        workpiece: "example-b".to_owned(),
        scope_revision: digest(2),
        awaiting_surface: None,
        withdrawn,
        cursor: None,
    }
}

fn bloom(members: Vec<MemberView>) -> BloomView {
    BloomView { id: digest(0), status: BloomStatus::Sealed, superseded_by: None, members }
}

fn commission(tip: DigestHex) -> CommissionShowView {
    CommissionShowView {
        intent: digest(0),
        status: "sealed".to_owned(),
        current_revision: Some(tip),
        current: None,
        approvals: Vec::new(),
    }
}

/// A policy whose only file-granular rules name `named`.
fn policy(named: &[&str]) -> ApprovalPolicy {
    ApprovalPolicy {
        default: Tier::Auto,
        rules: named.iter().map(|glob| ApprovalRule { glob: (*glob).to_owned(), tier: Tier::Human }).collect(),
    }
}

fn awaiting(revision: DigestHex, paths: &[(&str, &str)]) -> AwaitingSurfaceView {
    AwaitingSurfaceView {
        scope_revision: revision,
        paths: paths
            .iter()
            .map(|(path, reason)| SurfacePathRequest { path: (*path).to_owned(), reason: (*reason).to_owned() })
            .collect(),
        requests: 1,
    }
}

// Tripwire: a request bound to a revision the bloom has moved past is one a
// human already answered differently. Widening on it chains a successor off a
// revision the bloom no longer carries, and the seal refuses as stale — after
// the operator's key has signed and the commission tip has already moved.
#[test]
fn a_request_naming_a_revision_the_bloom_has_left_is_refused() {
    let stale = member(Some(awaiting(digest(9), &[("crates/example-b/src/lib.rs", "the caller")])));
    let error = request::binding_holds(&stale).expect_err("a stale binding is refused");
    assert!(error.to_string().contains("re-scope"), "the refusal names the remedy: {error}");

    let current = member(Some(awaiting(digest(1), &[("crates/example-b/src/lib.rs", "the caller")])));
    request::binding_holds(&current).expect("a request at the sealed revision holds");
}

// Tripwire: amending a member that asked for nothing, on the strength of
// nothing, is the shape that turns a declared surface into a suggestion. The
// operator may still do it — but only by naming the paths themselves.
#[test]
fn a_member_with_no_request_needs_explicit_paths() {
    let error = request::collect(&member(None), &[]).expect_err("no request and no --path is refused");
    assert!(error.to_string().contains("--path"), "the refusal names the way through: {error}");

    let forced = request::collect(&member(None), &["crates/example-b/**".to_owned()]).expect("--path is enough");
    assert_eq!(forced.globs, ["crates/example-b/**"]);
    assert_eq!(forced.requests, 0);
}

// Tripwire: the operator's extra paths union with the lane's rather than
// replacing them. Replacing would silently drop the path the member actually
// blocked on while reporting a granted amendment.
#[test]
fn the_lanes_paths_come_first_and_the_operators_union_in() {
    let requested = request::collect(
        &member(Some(awaiting(digest(1), &[("crates/example-b/src/lib.rs", "the caller")]))),
        &["crates/example-c/**".to_owned(), "crates/example-b/src/lib.rs".to_owned()],
    )
    .expect("a request plus extras");

    assert_eq!(requested.globs, ["crates/example-b/src/lib.rs", "crates/example-c/**"]);
    assert_eq!(requested.reasons.len(), 1, "only the lane's paths carry a reason");
}

// Tripwire: a withdrawn member is re-scoped for a later bloom, so its
// commission tip legitimately leaves the revision this bloom sealed it at.
// Checking it as a sibling refused every amendment in a bloom that had
// withdrawn anything, over a scope the successor seal never reads.
#[test]
fn a_withdrawn_siblings_moved_commission_does_not_refuse_the_amendment() {
    let left = bloom(vec![member(None), sibling(Some(WithdrawnView {}))]);
    assert!(siblings(&left, "example-a").next().is_none(), "a withdrawn member is no sibling to check");

    let standing = bloom(vec![member(None), sibling(None)]);
    let checked = siblings(&standing, "example-a").next().expect("a standing sibling is still checked");
    let error = sibling_is_sealable(checked, &commission(digest(7))).expect_err("a moved commission is refused");
    assert!(error.to_string().contains("stale"), "the refusal names what the seal would do: {error}");
}

// Tripwire: the seal door admits an entry naming one file only when a
// file-granular policy rule names that file, and a blocked lane asks for the
// file it stopped on. Sealing the raw path refuses the successor after the key
// has signed and the commission tip has already moved.
#[test]
fn a_file_the_policy_does_not_name_is_coarsened_to_the_glob_covering_it() {
    let policy = policy(&["docs/guide/testing.md"]);
    let widening = surface::widen(
        &policy,
        &["crates/example-b/**".to_owned()],
        &[
            "xtask/src/transform/verify/mod.rs".to_owned(),
            "crates/example-b/src/lib.rs".to_owned(),
            "crates/example-c/src/read.rs".to_owned(),
            "docs/guide/recipes/amending.md".to_owned(),
            ".github/workflows/ci.yml".to_owned(),
            // A file the policy names is what the policy meant to be named.
            "docs/guide/testing.md".to_owned(),
        ],
    )
    .expect("every request is inside the grammar");

    assert_eq!(
        widening.widened,
        [
            "crates/example-b/**",
            "xtask/**",
            "crates/example-c/**",
            "docs/guide/**",
            ".github/workflows/**",
            "docs/guide/testing.md",
        ],
    );
    assert_eq!(
        policy.unnamed_file_entries(&widening.widened),
        Vec::<String>::new(),
        "the surface the successor declares is one the seal door admits",
    );
}

// Tripwire: a revision sealed before the request arrived can already carry the
// raw file entry, so the successor has to widen what it inherits as well as
// what it adds — otherwise the amendment writes a fresh revision the seal door
// refuses for exactly the reason it refused the last one.
#[test]
fn a_file_entry_the_current_surface_carries_is_coarsened_too() {
    let current =
        ["xtask/src/transform/verify/mod.rs".to_owned(), "xtask/**".to_owned(), "crates/example-b/**".to_owned()];
    let widening = surface::widen(&policy(&[]), &current, &[]).expect("an empty request is inside the grammar");

    assert_eq!(widening.widened, ["xtask/**", "crates/example-b/**"]);
    assert_eq!(
        widening.inherited,
        [("xtask/src/transform/verify/mod.rs".to_owned(), "xtask/**".to_owned())],
        "the plan reports the entry it rewrote out from under the operator",
    );
}

// Tripwire: this command advances the tip itself, so a run that failed after
// the revision write leaves the tip ahead of the sealed revision through no
// human's doing. Reading that as a re-scope refuses every re-run, and the
// member — parked until a successor seals it — can never be finished.
#[test]
fn a_tip_this_command_wrote_is_sealed_rather_than_blamed_on_a_human() {
    let widened = ["crates/example-b/**".to_owned(), "xtask/**".to_owned()];

    assert_eq!(tip_standing(digest(1), digest(1), &widened[..1], &widened), TipStanding::Sealed);
    assert_eq!(tip_standing(digest(4), digest(1), &widened, &widened), TipStanding::AlreadyWidened);
    assert_eq!(
        tip_standing(digest(4), digest(1), &["crates/example-c/**".to_owned()], &widened),
        TipStanding::Moved,
        "a tip declaring anything else is still a scope somebody moved",
    );
}

// Tripwire: a signing seed another account on the host can read is a key that
// is no longer the operator's. A tool that signs with it anyway teaches the
// habit, and the habit is the whole exposure.
#[cfg(unix)]
#[test]
fn a_group_or_world_readable_seed_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch("loose-seed");
    let path = dir.join("seed");
    fs::write(&path, [3_u8; 32]).expect("write seed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set mode");

    let error = OperatorKey::load(KeyId("operator".into()), &path).expect_err("a loose seed is refused");
    assert!(error.to_string().contains("0600"), "the refusal names the fix: {error}");

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("set mode");
    OperatorKey::load(KeyId("operator".into()), &path).expect("a 0600 seed loads");
}

// Tripwire: the two seed spellings have to reach the same key, or an operator
// who stored theirs as hex signs with 32 bytes of ASCII and every approval
// they mint is refused by a verifier that cannot say why.
#[test]
fn a_hex_seed_and_the_raw_bytes_it_spells_mint_the_same_approval() {
    let dir = scratch("seed-forms");
    let raw_path = dir.join("raw");
    let hex_path = dir.join("hex");
    let raw = [0xAB_u8; 32];
    fs::write(&raw_path, raw).expect("write raw seed");
    fs::write(&hex_path, "ab".repeat(32)).expect("write hex seed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [&raw_path, &hex_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("set mode");
        }
    }

    let scope = aether_bloomery::Digest::from_bytes([5; 32]);
    let from_raw = OperatorKey::load(KeyId("operator".into()), &raw_path).expect("raw loads").approval_of(scope);
    let from_hex = OperatorKey::load(KeyId("operator".into()), &hex_path).expect("hex loads").approval_of(scope);

    assert_eq!(from_raw, from_hex);
}
