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

use aether_bloomery::{Ed25519KeyProvider, KeyId, Provenance, SignatureEnvelope, Statement};
use aether_data::wire::to_vec;
use ed25519_dalek::{Signer, SigningKey};

use super::kinds::VerifyResult;
use super::runtime::{SigningCapabilityState, parse_allowlist};

/// A deterministic signing key from a fixed seed.
fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A capability state trusting exactly `signer` at `key`'s public half.
fn state(signer: &str, key: &SigningKey) -> SigningCapabilityState {
    let allowlist = BTreeMap::from([(KeyId(signer.to_owned()), key.verifying_key())]);
    SigningCapabilityState::new(Ed25519KeyProvider::new(allowlist))
}

/// The wire-encoded author-signed statement over `words` by `signer` using `key`.
fn signed(signer: &str, key: &SigningKey, words: &[u8]) -> Vec<u8> {
    let statement = Statement {
        words: words.to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(words).to_bytes().to_vec(),
        }),
        parents: vec![],
    };
    to_vec(&statement).unwrap()
}

/// The 64-char hex of a signing key's public half — the allowlist config form.
fn key_hex(key: &SigningKey) -> String {
    key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn an_authorized_signers_genuine_signature_verifies() {
    let key = signing_key(7);
    let reply = state("owner", &key).verify(&signed("owner", &key, b"answer: choose A"));
    assert_eq!(reply, VerifyResult::Ok { verified: true }, "an allowlisted signer's real signature admits");
}

#[test]
fn a_non_allowlisted_signer_is_rejected() {
    // The signature is genuine, but the signer id is not in the allowlist — the
    // fail-closed gate returns `verified: false`, not an error.
    let key = signing_key(7);
    let reply = state("owner", &key).verify(&signed("intruder", &key, b"do the thing"));
    assert_eq!(reply, VerifyResult::Ok { verified: false }, "a signer absent from the allowlist does not verify");
}

#[test]
fn a_non_statement_body_is_an_error_not_a_false_verdict() {
    // Undecodable bytes are `Err` — distinct from a well-formed statement that
    // simply does not verify, so the gate can tell "malformed" from "rejected".
    let reply = state("owner", &signing_key(7)).verify(b"not a wire statement");
    assert!(matches!(reply, VerifyResult::Err { .. }), "undecodable bytes surface as Err, got {reply:?}");
}

#[test]
fn parse_allowlist_reads_the_config_entries() {
    let key = signing_key(3);
    let config = format!("owner:{}", key_hex(&key));
    let parsed = parse_allowlist(Some(&config)).unwrap();
    assert_eq!(parsed.len(), 1, "one entry parsed");
    assert_eq!(parsed.get(&KeyId("owner".to_owned())), Some(&key.verifying_key()), "the pubkey round-trips");
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
