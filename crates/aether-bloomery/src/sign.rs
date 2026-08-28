//! Statement signing — the verification contract, not the custody (ADR-0149
//! §The value vocabulary, §The boundary).
//!
//! ADR-0149 ships the statement, manifest, and receipt shapes from the start
//! because everything downstream binds to them and they are what make replay
//! and audit possible. Verification rides the [`KeyProvider`] port so custody
//! can evolve behind it without a core change:
//!
//! - [`FakeKeyProvider`] — the always-valid stub, kept as a test / no-key
//!   helper (a solo bin with no adversary to reject).
//! - [`Ed25519KeyProvider`] — the real ed25519 verifier against an
//!   authorized-signer allowlist (ADR-0149 step 3). Native-only: the private
//!   keys and the allowlist live in the host's `aether.signing` capability
//!   (ADR-0150, credentials never leave the machine), and the wasm control
//!   actor holds no key material — so the concrete provider is constructed
//!   host-side, never in the guest.
//!
//! The fail-closed prompt closure ([`crate::manifest`]) is structural and
//! enforced from day one regardless of which provider is wired.
//!
//! # What a signature covers
//!
//! Not the words alone. [`authorization_message`] is the message every author
//! signature verifies against: the digest of an [`AuthorityDoor`], the request
//! digest the signature is bound to, and the asserted words together (ADR-0182).
//! Putting the binding inside the signed bytes is what makes a captured envelope
//! authorize the one request it was made for — a statement's `parents` sit
//! outside the signature and can be rewritten by whoever holds it, so a door
//! that bound structurally alone turned one legitimate authorization into a
//! standing credential.

#[cfg(not(target_arch = "wasm32"))]
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
#[cfg(not(target_arch = "wasm32"))]
use std::error::Error;
#[cfg(not(target_arch = "wasm32"))]
use std::fmt;
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
#[cfg(not(target_arch = "wasm32"))]
use std::io;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::str;

#[cfg(not(target_arch = "wasm32"))]
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::KeyId;
use crate::values::Tier;

/// Which door an author signature authorizes (ADR-0182).
///
/// A closed enum hashed into the signed subject, so an envelope minted for one
/// door never verifies at another even where the words and the binding coincide.
/// Adding a door is a variant, and the variant is what keeps its envelopes
/// separate from every existing door's.
///
/// **Variant order is part of the signed subject.** The discriminant is what
/// serde encodes into the authorization [`authorization_message`] hashes, so
/// reordering these variants — or inserting one anywhere but the end — silently
/// changes the message every past signature was minted over and invalidates all
/// of them, with no compile error and no decode failure to announce it. Append
/// new doors; never reorder.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AuthorityDoor {
    /// Approving a member's scope revision at seal time, bound to that
    /// revision's digest.
    Approve,
    /// Adopting an answer to a parked question, bound to the question's digest.
    Answer,
    /// Releasing an orphaned claim ref, bound to the release request's digest
    /// (ADR-0179).
    OrphanClaimRelease,
    /// Grounding a prompt-manifest instruction slot, bound to the artifact the
    /// signature grounds. Not a request door: nothing acts on it, so it
    /// authorizes no mutation — the variant exists so the closure walk's
    /// cryptographic check cannot borrow a request door's envelope, and
    /// [`ground_instruction`](crate::manifest) enforces that by grounding only
    /// on a recorded authority whose door is this one.
    Ground,
    /// Cancelling an open commission, bound to that commission's intent digest.
    /// Appended past [`Self::Ground`] so existing door discriminants stay put.
    Cancel,
    /// Accepting an architecture decision record, bound to that ADR's digest.
    /// Appended past [`Self::Cancel`] so existing door discriminants stay put.
    Accept,
    /// Putting a commission stranded outside `open` back into the line, bound
    /// to that commission's intent digest. Its own door rather than
    /// [`Self::Cancel`]'s: restoring and retiring are opposite acts, and one
    /// envelope must not be replayable at the other. Appended past
    /// [`Self::Accept`] so existing door discriminants stay put.
    Reopen,
    /// Proposing an operator change onto the day's branch, bound to the
    /// proposal's digest. Appended past [`Self::Reopen`] so existing door
    /// discriminants stay put.
    Propose,
}

/// The subject an author signature actually covers (ADR-0182): the door, the
/// request it binds, and the asserted words together.
///
/// Private because it is a hashing subject rather than a stored value — callers
/// reach it through [`authorization_message`], which is the only way to produce
/// the message a [`KeyProvider`] verifies against.
#[derive(Serialize)]
struct Authorization<'a> {
    /// Which door this signature authorizes.
    door: AuthorityDoor,
    /// The exact request digest this signature is good for.
    binding: Digest,
    /// The asserted bytes.
    words: &'a [u8],
}

impl ContentAddressed for Authorization<'_> {
    const DOMAIN: &'static str = "aether.bloomery.authorization";
}

/// The message an author signature over `words` at `door` bound to `binding`
/// must cover (ADR-0182).
///
/// The binding is a parameter with no default and there is no verification path
/// over words alone, so a signature authorizes exactly one request at exactly
/// one door: rewriting a statement's `parents` changes the artifact's address
/// without producing a signature that verifies against the new target.
#[must_use]
pub fn authorization_message(door: AuthorityDoor, binding: Digest, words: &[u8]) -> Digest {
    digest_of(&Authorization { door, binding, words })
}

/// An author signature over exact bytes: the assertion "this signer asserted
/// these bytes for this purpose". The only provenance that can become
/// *instruction* (ADR-0149 §The value vocabulary).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// The asserting signer's key identity.
    pub signer: KeyId,
    /// The signature bytes over the signed message. Opaque to the core — a
    /// [`KeyProvider`] interprets them.
    pub signature: Vec<u8>,
}

/// Verifies author signatures. The private keys never enter this crate (nor
/// wasm, in the actor slice) — the host's `signing` capability owns custody
/// (ADR-0149 §The boundary); this trait is the pure verification contract
/// the reducer and manifest assembly call through.
pub trait KeyProvider {
    /// Does `envelope` verify as a signature by its named signer over
    /// `message`?
    fn verify(&self, envelope: &SignatureEnvelope, message: &[u8]) -> bool;

    /// The highest [`Tier`] `signer` is authorized to approve at, or `None`
    /// when the signer is not in the key policy at all.
    ///
    /// The second half of the key policy, and the one [`Self::verify`] cannot
    /// answer (#5324). A signature check proves *who* asserted the words; it
    /// says nothing about whether that signer may stand in for the reader the
    /// tier policy asked for. Without this, one allowlist entry authorizes
    /// every tier, so an operator key signs a `human`-tier surface and the gate
    /// admits it exactly as it would an `auto` one — human tier enforced by the
    /// human declining to sign rather than by the machine.
    ///
    /// Required rather than defaulted: a provider that cannot state a ceiling
    /// has no business answering the approve gate, and a default would let a
    /// new provider inherit "authorized at every tier" by writing nothing.
    fn tier_ceiling(&self, signer: &KeyId) -> Option<Tier>;
}

/// The v1 stub: every well-formed envelope verifies. With one operator there
/// is no adversary to reject, so this stands in until real key custody lands
/// (ADR-0149 §The value vocabulary). Downstream code binds to the trait, so
/// swapping in a real provider is a host wiring change, not a core change.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeKeyProvider;

impl KeyProvider for FakeKeyProvider {
    fn verify(&self, _envelope: &SignatureEnvelope, _message: &[u8]) -> bool {
        true
    }

    /// Every signer is authorized to the top of the ladder, matching the stub's
    /// "every well-formed envelope verifies" contract. A stub that admitted the
    /// signature and then refused the tier would fail every above-auto path in
    /// the no-key configuration this exists to serve.
    fn tier_ceiling(&self, _signer: &KeyId) -> Option<Tier> {
        Some(Tier::Human)
    }
}

/// The real verifier: an ed25519 signature check against an in-memory
/// allowlist of authorized signers (ADR-0149 step 3). A signature verifies
/// only when its named [`KeyId`] is in the allowlist, its bytes are a 64-byte
/// ed25519 signature, and that signature checks against the allowlisted public
/// key over the exact `message` — every other case is a rejection, so the gate
/// is fail-closed. Custody of the allowlist and the private keys is the host's
/// `aether.signing` capability (ADR-0150, credentials never leave the machine);
/// this impl is pure, deterministic, and I/O-free — it belongs with the trait
/// it implements and is reused by the core manifest-closure gate's tests.
///
/// Native-only: the wasm control actor holds no key material and never
/// constructs a provider (ADR-0149 §The boundary), so `ed25519-dalek` stays out
/// of the wasm cdylib build.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct Ed25519KeyProvider {
    allowlist: BTreeMap<KeyId, AuthorizedSigner>,
}

/// One allowlist entry: the public key a signer signs with, and the highest
/// [`Tier`] that signer may approve at (#5324).
///
/// The two travel together because verifying a signature and admitting an
/// approval are the same decision seen from two sides — a key with no stated
/// ceiling is a key whose authority nobody wrote down, and the gate that reads
/// one without the other admits every tier to every allowlisted signer.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
pub struct AuthorizedSigner {
    /// The 32-byte ed25519 verifying key this signer signs with.
    pub key: VerifyingKey,
    /// The highest tier this signer may approve at. A statement over a surface
    /// that resolves above this is refused with both tiers named, however good
    /// the signature is.
    pub ceiling: Tier,
}

#[cfg(not(target_arch = "wasm32"))]
impl Ed25519KeyProvider {
    /// Build a provider over the allowlist mapping each [`KeyId`] to its
    /// [`AuthorizedSigner`] — the
    /// set of signers authorized to sign in a person's stead, each with the tier
    /// it may sign up to (ADR-0151 key policy, #5324). An empty allowlist
    /// verifies nothing and authorizes no tier.
    #[must_use]
    pub fn new(allowlist: BTreeMap<KeyId, AuthorizedSigner>) -> Self {
        Self { allowlist }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl KeyProvider for Ed25519KeyProvider {
    fn verify(&self, envelope: &SignatureEnvelope, message: &[u8]) -> bool {
        let Some(signer) = self.allowlist.get(&envelope.signer) else {
            return false;
        };
        let Ok(bytes) = <[u8; 64]>::try_from(envelope.signature.as_slice()) else {
            return false;
        };
        signer.key.verify_strict(message, &Signature::from_bytes(&bytes)).is_ok()
    }

    fn tier_ceiling(&self, signer: &KeyId) -> Option<Tier> {
        self.allowlist.get(signer).map(|signer| signer.ceiling)
    }
}

/// Mint an author signature at `door`, bound to `binding`, over `words` — the
/// inverse of [`authorization_message`] plus [`KeyProvider::verify`].
///
/// The private half of key custody is the operator's, not this crate's: the
/// coordinator holds no signing keys, so nothing on the host calls this. It is
/// here because it must not drift from [`authorization_message`], and a signer
/// that lives outside the tree is a signer that can disagree with the verifier
/// without anything failing to compile — which is how an unreproducible
/// approval recipe happens.
///
/// ed25519 signing is deterministic (RFC 8032), so the same seed over the same
/// message re-mints byte-identical bytes. Every caller's idempotency rests on
/// that: a re-run produces the same statement digest and the store no-ops.
///
/// Native-only for the same reason [`Ed25519KeyProvider`] is: the wasm control
/// actor holds no key material.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn sign_authorization(
    signer: KeyId,
    seed: &[u8; 32],
    door: AuthorityDoor,
    binding: Digest,
    words: &[u8],
) -> SignatureEnvelope {
    let key = SigningKey::from_bytes(seed);
    let signature = key.sign(authorization_message(door, binding, words).as_bytes()).to_bytes().to_vec();
    SignatureEnvelope { signer, signature }
}

/// The operator's signing seed, loaded from a file on the operator's host.
///
/// The coordinator holds no private keys, so every approval at every tier
/// needs a signature minted here. Custody is the operator's, and the file mode
/// check is the one thing this can do about it.
#[cfg(not(target_arch = "wasm32"))]
pub struct OperatorKey {
    /// The allowlist identity this seed signs as.
    pub signer: KeyId,
    seed: [u8; 32],
}

/// Hand-written rather than derived: the seed is a private signing key, and a
/// derived `Debug` would put it in every panic message and test failure that
/// prints one.
#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for OperatorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorKey").field("signer", &self.signer).field("seed", &"<redacted>").finish()
    }
}

/// Why [`OperatorKey::load`] refused a seed file.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub enum OperatorKeyError {
    /// The seed file could not be read.
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// The underlying IO error.
        source: io::Error,
    },
    /// The seed file could not be stat'd for its mode.
    Stat {
        /// Path that failed to stat.
        path: PathBuf,
        /// The underlying IO error.
        source: io::Error,
    },
    /// Group or other can read the seed file.
    LooseMode {
        /// Path whose mode was too open.
        path: PathBuf,
        /// `mode & 0o777` as reported by the filesystem.
        mode: u32,
    },
    /// The file was neither 32 raw bytes nor 64 hex characters.
    InvalidSeed {
        /// Path whose contents were not a seed.
        path: PathBuf,
    },
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Display for OperatorKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, .. } => write!(f, "read signing seed {}", path.display()),
            Self::Stat { path, .. } => write!(f, "stat signing seed {}", path.display()),
            Self::LooseMode { path, mode } => {
                write!(f, "signing seed {} is mode {:o}; make it 0600 before signing with it", path.display(), mode)
            }
            Self::InvalidSeed { path } => {
                write!(f, "signing seed {} is neither 32 raw bytes nor 64 hex", path.display())
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Error for OperatorKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Stat { source, .. } => Some(source),
            Self::LooseMode { .. } | Self::InvalidSeed { .. } => None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl OperatorKey {
    /// Load a 32-raw-byte or 64-hex seed from `path`.
    ///
    /// Refuses a file any group or other can read: a signing seed readable by
    /// another account on the host is a key that is no longer the operator's,
    /// and a tool that shrugs at that teaches the habit.
    ///
    /// # Errors
    ///
    /// [`OperatorKeyError`] when the file cannot be read, is group- or
    /// other-readable, or is neither 32 raw bytes nor 64 hex characters.
    pub fn load(signer: KeyId, path: &Path) -> Result<Self, OperatorKeyError> {
        let bytes = fs::read(path).map_err(|source| OperatorKeyError::Read { path: path.to_owned(), source })?;
        refuse_loose_mode(path)?;
        let seed = decode_seed(&bytes).ok_or_else(|| OperatorKeyError::InvalidSeed { path: path.to_owned() })?;
        Ok(Self { signer, seed })
    }

    /// The 32-byte ed25519 seed this key signs with.
    #[must_use]
    pub const fn seed(&self) -> &[u8; 32] {
        &self.seed
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_seed(bytes: &[u8]) -> Option<[u8; 32]> {
    if let Ok(raw) = <[u8; 32]>::try_from(bytes) {
        return Some(raw);
    }
    let text = str::from_utf8(bytes).ok()?.trim();
    if text.len() != 64 {
        return None;
    }
    let mut seed = [0_u8; 32];
    for (index, slot) in seed.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(seed)
}

#[cfg(all(not(target_arch = "wasm32"), unix))]
fn refuse_loose_mode(path: &Path) -> Result<(), OperatorKeyError> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = fs::metadata(path).map_err(|source| OperatorKeyError::Stat { path: path.to_owned(), source })?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(OperatorKeyError::LooseMode { path: path.to_owned(), mode: mode & 0o777 });
    }
    Ok(())
}

#[cfg(all(not(target_arch = "wasm32"), not(unix)))]
fn refuse_loose_mode(_path: &Path) -> Result<(), OperatorKeyError> {
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use alloc::collections::BTreeMap;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;

    use ed25519_dalek::{Signer, SigningKey};

    use super::{
        AuthorityDoor, AuthorizedSigner, Ed25519KeyProvider, KeyProvider, OperatorKey, SignatureEnvelope,
        authorization_message, sign_authorization,
    };
    use crate::digest::Digest;
    use crate::ids::KeyId;
    use crate::values::Tier;

    /// A distinct binding digest per seed — two requests to sign for.
    fn binding(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    /// A deterministic signing key from a fixed 32-byte seed — no rng, so the
    /// test is reproducible.
    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A provider trusting exactly `signer` at `key`'s public half, authorized
    /// to the top of the tier ladder.
    fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
        provider_at(signer, key, Tier::Human)
    }

    /// A provider trusting exactly `signer` at `key`'s public half, authorized
    /// no higher than `ceiling`.
    fn provider_at(signer: &str, key: &SigningKey, ceiling: Tier) -> Ed25519KeyProvider {
        Ed25519KeyProvider::new(BTreeMap::from([(
            KeyId(signer.into()),
            AuthorizedSigner { key: key.verifying_key(), ceiling },
        )]))
    }

    /// An author-signature envelope over `message` by `signer` using `key`.
    fn envelope(signer: &str, key: &SigningKey, message: &[u8]) -> SignatureEnvelope {
        SignatureEnvelope { signer: KeyId(signer.into()), signature: key.sign(message).to_bytes().to_vec() }
    }

    #[test]
    fn a_genuine_signature_by_an_allowlisted_signer_verifies() {
        let key = signing_key(7);
        let message = b"answer: choose A";
        assert!(provider("owner", &key).verify(&envelope("owner", &key, message), message));
    }

    #[test]
    fn a_signer_absent_from_the_allowlist_is_rejected() {
        // The signature is genuine, but "intruder" is not an authorized signer —
        // fail-closed on the allowlist, not just on the crypto.
        let key = signing_key(7);
        let message = b"do the thing";
        assert!(!provider("owner", &key).verify(&envelope("intruder", &key, message), message));
    }

    #[test]
    fn a_signature_by_the_wrong_key_is_rejected() {
        // Right signer id, but signed with a different key than the allowlist
        // holds — the ed25519 check fails.
        let message = b"do the thing";
        let bad = envelope("owner", &signing_key(9), message);
        assert!(!provider("owner", &signing_key(7)).verify(&bad, message));
    }

    #[test]
    fn tampered_words_do_not_verify() {
        // A signature over the original words does not verify over altered ones.
        let key = signing_key(7);
        let signed = envelope("owner", &key, b"answer: choose A");
        assert!(!provider("owner", &key).verify(&signed, b"answer: choose B"));
    }

    #[test]
    fn a_signature_minted_for_one_door_does_not_verify_at_another() {
        // The whole point of hashing AuthorityDoor into the subject (ADR-0182):
        // an answer envelope must not release an orphaned claim ref, even when
        // the operator signed the same words for the same request digest.
        let key = signing_key(7);
        let words = b"yes";
        let signed =
            envelope("owner", &key, authorization_message(AuthorityDoor::Answer, binding(1), words).as_bytes());

        assert!(
            !provider("owner", &key).verify(
                &signed,
                authorization_message(AuthorityDoor::OrphanClaimRelease, binding(1), words).as_bytes()
            )
        );
    }

    #[test]
    fn a_signature_minted_for_one_binding_does_not_verify_at_another() {
        // The replay this ADR closes: re-pointing a captured envelope at a
        // second request digest yields no verifying signature, so the constant
        // words a release door signs are no longer a standing credential.
        let key = signing_key(7);
        let words = b"release orphan bloomery claim";
        let signed = envelope(
            "owner",
            &key,
            authorization_message(AuthorityDoor::OrphanClaimRelease, binding(1), words).as_bytes(),
        );

        assert!(
            !provider("owner", &key).verify(
                &signed,
                authorization_message(AuthorityDoor::OrphanClaimRelease, binding(2), words).as_bytes()
            )
        );
    }

    #[test]
    fn a_malformed_signature_is_rejected_not_a_panic() {
        // Signature bytes that are not 64 bytes long are refused at the length
        // check rather than panicking in the ed25519 decode.
        let key = signing_key(7);
        let message = b"do the thing";
        let malformed = SignatureEnvelope { signer: KeyId("owner".into()), signature: vec![1, 2, 3] };
        assert!(!provider("owner", &key).verify(&malformed, message));
    }

    // Tripwire: the in-tree signer and the in-tree verifier have to agree on
    // the ADR-0182 message. A signer that composed the subject even slightly
    // differently would mint approvals no coordinator accepts, with nothing
    // failing to compile to announce it — the exact hazard an out-of-tree
    // signing tool carries.
    #[test]
    fn a_minted_authorization_verifies_at_its_own_door_and_nowhere_else() {
        let seed = [7_u8; 32];
        let key = SigningKey::from_bytes(&seed);
        let words = b"the scope revision";
        let envelope = sign_authorization(KeyId("operator".into()), &seed, AuthorityDoor::Approve, binding(1), words);

        let keys = provider("operator", &key);
        assert!(keys.verify(&envelope, authorization_message(AuthorityDoor::Approve, binding(1), words).as_bytes()));
        assert!(
            !keys.verify(&envelope, authorization_message(AuthorityDoor::Answer, binding(1), words).as_bytes()),
            "an Approve envelope must not open the Answer door",
        );
        assert!(
            !keys.verify(&envelope, authorization_message(AuthorityDoor::Approve, binding(2), words).as_bytes()),
            "an envelope authorizes one binding and nothing else",
        );
    }

    // Tripwire: every caller's idempotency rests on ed25519 determinism — a
    // re-run has to re-mint the same statement digest so the store no-ops
    // rather than writing a second approval.
    #[test]
    fn signing_the_same_subject_twice_produces_identical_bytes() {
        let seed = [9_u8; 32];
        let mint = || sign_authorization(KeyId("operator".into()), &seed, AuthorityDoor::Approve, binding(3), b"w");
        assert_eq!(mint(), mint());
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("aether-sign-{tag}-{}", process::id()));
        fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create scratch dir: {error}"));
        dir
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
        fs::write(&path, [3_u8; 32]).unwrap_or_else(|error| panic!("write seed: {error}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("set mode: {error}"));

        let error = OperatorKey::load(KeyId("operator".into()), &path).expect_err("a loose seed is refused");
        assert!(error.to_string().contains("0600"), "the refusal names the fix: {error}");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("set mode: {error}"));
        OperatorKey::load(KeyId("operator".into()), &path).expect("a 0600 seed loads");
    }

    // Tripwire: the two seed spellings have to reach the same key, or an operator
    // who stored theirs as hex signs with 32 bytes of ASCII and every approval
    // they mint is refused by a verifier that cannot say why.
    #[test]
    fn a_hex_seed_and_the_raw_bytes_it_spells_decode_to_the_same_key() {
        let dir = scratch("seed-forms");
        let raw_path = dir.join("raw");
        let hex_path = dir.join("hex");
        let raw = [0xAB_u8; 32];
        fs::write(&raw_path, raw).unwrap_or_else(|error| panic!("write raw seed: {error}"));
        fs::write(&hex_path, "ab".repeat(32)).unwrap_or_else(|error| panic!("write hex seed: {error}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&raw_path, &hex_path] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                    .unwrap_or_else(|error| panic!("set mode: {error}"));
            }
        }

        let from_raw = OperatorKey::load(KeyId("operator".into()), &raw_path).expect("raw loads");
        let from_hex = OperatorKey::load(KeyId("operator".into()), &hex_path).expect("hex loads");
        assert_eq!(from_raw.seed(), from_hex.seed());
    }
}
