//! Handler tests for [`SigningCapabilityState`] (ADR-0149 step 3).
//!
//! Each test drives the state's inherent [`verify`](SigningCapabilityState::verify)
//! — the exact method the `on_verify` handler delegates to — plus the
//! [`parse_allowlist`] boot path, over a known ed25519 keypair (no rng, so the
//! tests are reproducible). Tripwire: the decode → verify → reply mapping and
//! the fail-closed allowlist gate are this crate's own logic, not a derive or a
//! passthrough.

#![allow(clippy::unwrap_used)]
// The allowlist-config hex helper mirrors the answer-gate test's own
// `key-id:hex` rendering — test-harness ergonomics, not a hot path.
#![allow(clippy::format_collect)]

use std::collections::BTreeMap;

use aether_bloomery::testing::digest as binding;
use aether_bloomery::{
    AuthorityDoor, AuthorizedSigner, Digest, Ed25519KeyProvider, KeyId, Provenance, SignatureEnvelope, Statement, Tier,
    authorization_message,
};
use aether_data::wire::to_vec;
use ed25519_dalek::{Signer, SigningKey};

use super::kinds::{VerifyResult, authority_bytes};
use super::runtime::{SigningCapabilityState, parse_allowlist};

/// A deterministic signing key from a fixed seed.
fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A capability state trusting exactly `signer` at `key`'s public half,
/// authorized to the top of the tier ladder.
fn state(signer: &str, key: &SigningKey) -> SigningCapabilityState {
    state_at(signer, key, Tier::Human)
}

/// A capability state trusting exactly `signer` at `key`'s public half,
/// authorized no higher than `ceiling`.
fn state_at(signer: &str, key: &SigningKey, ceiling: Tier) -> SigningCapabilityState {
    let allowlist =
        BTreeMap::from([(KeyId(signer.to_owned()), AuthorizedSigner { key: key.verifying_key(), ceiling })]);
    SigningCapabilityState::new(Ed25519KeyProvider::new(allowlist))
}

/// The wire-encoded author-signed statement over `words` by `signer` using
/// `key`, signed as authority for `door` bound to `bound` (ADR-0182).
fn signed(signer: &str, key: &SigningKey, words: &[u8], door: AuthorityDoor, bound: Digest) -> Vec<u8> {
    let message = authorization_message(door, bound, words);
    let statement = Statement {
        words: words.to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents: vec![],
    };
    to_vec(&statement).unwrap()
}

/// The 64-char hex of a signing key's public half — the allowlist config form.
fn key_hex(key: &SigningKey) -> String {
    aether_bloomery::encode_hex(&key.verifying_key().to_bytes())
}

#[test]
fn an_authorized_signers_genuine_signature_verifies() {
    let key = signing_key(7);
    let statement = signed("owner", &key, b"answer: choose A", AuthorityDoor::Answer, binding(1));

    let reply = state("owner", &key).verify(&statement, &authority_bytes(AuthorityDoor::Answer, binding(1)), None);

    assert_eq!(reply, VerifyResult::Ok { verified: true }, "an allowlisted signer's real signature admits");
}

#[test]
fn a_non_allowlisted_signer_is_rejected() {
    // The signature is genuine, but the signer id is not in the allowlist — the
    // fail-closed gate returns `verified: false`, not an error.
    let key = signing_key(7);
    let statement = signed("intruder", &key, b"do the thing", AuthorityDoor::Answer, binding(1));

    let reply = state("owner", &key).verify(&statement, &authority_bytes(AuthorityDoor::Answer, binding(1)), None);

    assert_eq!(reply, VerifyResult::Ok { verified: false }, "a signer absent from the allowlist does not verify");
}

#[test]
fn a_genuine_signature_presented_under_another_binding_is_rejected() {
    // The replay this capability now refuses (ADR-0182): the statement, the
    // signer, and the words are all genuine, and only the request the caller
    // supplies differs from the one the operator signed for. Before the binding
    // was signed this verified, and the door's only protection was a `parents`
    // check over a field the envelope's holder can rewrite.
    let key = signing_key(7);
    let statement = signed("owner", &key, b"yes", AuthorityDoor::Answer, binding(1));

    let reply = state("owner", &key).verify(&statement, &authority_bytes(AuthorityDoor::Answer, binding(2)), None);

    assert_eq!(reply, VerifyResult::Ok { verified: false }, "a signature bound to another request does not verify");
}

#[test]
fn a_genuine_signature_presented_at_another_door_is_rejected() {
    // Domain separation between doors: even at the same binding, an envelope
    // minted to answer a question must not authorize an orphan-claim release.
    let key = signing_key(7);
    let statement = signed("owner", &key, b"yes", AuthorityDoor::Answer, binding(1));

    let reply =
        state("owner", &key).verify(&statement, &authority_bytes(AuthorityDoor::OrphanClaimRelease, binding(1)), None);

    assert_eq!(reply, VerifyResult::Ok { verified: false }, "a signature minted for another door does not verify");
}

#[test]
fn a_non_statement_body_is_an_error_not_a_false_verdict() {
    // Undecodable bytes are `Err` — distinct from a well-formed statement that
    // simply does not verify, so the gate can tell "malformed" from "rejected".
    let reply = state("owner", &signing_key(7)).verify(
        b"not a wire statement",
        &authority_bytes(AuthorityDoor::Answer, binding(1)),
        None,
    );

    assert!(matches!(reply, VerifyResult::Err { .. }), "undecodable bytes surface as Err, got {reply:?}");
}

#[test]
fn an_undecodable_authority_is_an_error_not_a_false_verdict() {
    // An authority that does not decode leaves no request to check the signature
    // against, so it answers exactly as an undecodable statement does rather
    // than reporting a verdict it never reached.
    let key = signing_key(7);
    let statement = signed("owner", &key, b"yes", AuthorityDoor::Answer, binding(1));

    let reply = state("owner", &key).verify(&statement, b"not a wire authority", None);

    assert!(matches!(reply, VerifyResult::Err { .. }), "an undecodable authority surfaces as Err, got {reply:?}");
}

#[test]
fn parse_allowlist_reads_the_config_entries() {
    let key = signing_key(3);
    let config = format!("owner:{}", key_hex(&key));
    let parsed = parse_allowlist(Some(&config)).unwrap();
    assert_eq!(parsed.len(), 1, "one entry parsed");
    let entry = parsed.get(&KeyId("owner".to_owned())).expect("the owner entry parsed");
    assert_eq!(entry.key, key.verifying_key(), "the pubkey round-trips");
    assert_eq!(entry.ceiling, Tier::Auto, "an entry that states no tier authorizes the bottom of the ladder");
}

#[test]
fn parse_allowlist_unset_is_the_empty_fail_closed_set() {
    assert!(parse_allowlist(None).unwrap().is_empty(), "no config → no authorized signers");
}

#[test]
fn parse_allowlist_rejects_a_malformed_entry() {
    // A boot with a malformed hex key must fail loudly rather than silently drop
    // the signer (which would read as "not authorized" — a silent downgrade).
    assert!(parse_allowlist(Some("owner:not-hex")).is_err(), "malformed hex is a boot error");
    assert!(parse_allowlist(Some("owner-without-a-colon")).is_err(), "a missing separator is a boot error");
}

#[test]
fn parse_allowlist_rejects_a_duplicate_key_id() {
    // Two entries for the same key-id are ambiguous; keeping the last would
    // silently override the first signer's key — a silent trust change, so it is
    // a boot error rather than a last-wins resolution.
    let key = signing_key(3);
    let config = format!("owner:{a},owner:{a}", a = key_hex(&key));
    assert!(parse_allowlist(Some(&config)).is_err(), "a duplicate key-id is a boot error");
}

#[test]
fn parse_allowlist_reads_a_stated_tier_ceiling() {
    let key = signing_key(3);
    let parsed = parse_allowlist(Some(&format!("owner:{}:human,bot:{}:judge", key_hex(&key), key_hex(&key)))).unwrap();

    assert_eq!(parsed.get(&KeyId("owner".to_owned())).unwrap().ceiling, Tier::Human, "owner is authorized to human");
    assert_eq!(parsed.get(&KeyId("bot".to_owned())).unwrap().ceiling, Tier::Judge, "bot stops at judge");
}

#[test]
fn parse_allowlist_rejects_an_unknown_tier_spelling() {
    // A tier nobody can read must not resolve to a guess. Guessing high hands
    // out authority the operator never wrote; guessing low is the silent
    // downgrade the duplicate-key-id rule already refuses. So it is a boot
    // error naming what was written.
    let config = format!("owner:{}:owner", key_hex(&signing_key(3)));

    assert!(parse_allowlist(Some(&config)).is_err(), "an unknown tier spelling is a boot error");
}

#[test]
fn a_signer_below_the_required_tier_is_refused_with_both_tiers() {
    // The #5324 hole: an allowlisted operator key signing a human-tier surface.
    // The signature is genuine and the door and binding are the ones it was
    // minted for — only the signer's authority falls short, so the refusal has
    // to be its own verdict naming both tiers rather than `verified: false`.
    let key = signing_key(7);
    let statement = signed("operator", &key, b"approve", AuthorityDoor::Approve, binding(1));

    let reply = state_at("operator", &key, Tier::Judge).verify(
        &statement,
        &authority_bytes(AuthorityDoor::Approve, binding(1)),
        Some(Tier::Human),
    );

    assert_eq!(
        reply,
        VerifyResult::BelowTier { required: Tier::Human, ceiling: Tier::Judge },
        "a judge-ceiling key must not approve a human-tier surface"
    );
}

#[test]
fn a_signer_at_or_above_the_required_tier_verifies() {
    // The other side of the same gate: authority that reaches the required tier
    // admits, so the binding refuses too little rather than everything.
    let key = signing_key(7);
    let statement = signed("owner", &key, b"approve", AuthorityDoor::Approve, binding(1));
    let authority = authority_bytes(AuthorityDoor::Approve, binding(1));

    let exact = state_at("owner", &key, Tier::Judge).verify(&statement, &authority, Some(Tier::Judge));
    let above = state_at("owner", &key, Tier::Human).verify(&statement, &authority, Some(Tier::Judge));

    assert_eq!(exact, VerifyResult::Ok { verified: true }, "a ceiling equal to the requirement admits");
    assert_eq!(above, VerifyResult::Ok { verified: true }, "a ceiling above the requirement admits");
}

#[test]
fn a_bad_signature_is_a_false_verdict_before_any_tier_answer() {
    // Order matters: a ceiling lookup on a merely claimed signer would answer
    // for a key the caller has not proven it holds, turning the tier refusal
    // into an oracle over the allowlist. An unverifiable signature must reach
    // `verified: false` and never `BelowTier`, however low the claimed signer's
    // ceiling is.
    let key = signing_key(7);
    let statement = signed("intruder", &key, b"approve", AuthorityDoor::Approve, binding(1));

    let reply = state_at("owner", &key, Tier::Auto).verify(
        &statement,
        &authority_bytes(AuthorityDoor::Approve, binding(1)),
        Some(Tier::Human),
    );

    assert_eq!(reply, VerifyResult::Ok { verified: false }, "the signature is judged before the ceiling is consulted");
}
