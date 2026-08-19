//! The `Ed25519KeyProvider`-backed runtime for [`SigningCapability`] (ADR-0149
//! step 3, ADR-0150).
//!
//! State holds the real verifier over the host-local authorized-signer
//! allowlist. `init` parses the `key-id:hex-public-key` config into the
//! provider, failing the boot on a malformed entry rather than silently
//! trusting a smaller set (a dropped signer would read as "not authorized" — a
//! silent security downgrade). The single `#[handler::single] on_verify`
//! delegates to the inherent [`verify`](SigningCapabilityState::verify), which
//! decodes the wire `Statement` and the caller-supplied authority, then runs the
//! pure
//! [`Statement::verify_authority`](aether_bloomery::Statement::verify_authority)
//! against the provider — so the "only author signatures verify" rule stays in
//! core and the test drives the exact method the handler does. The authority the
//! caller supplies is what the signature must be bound to (ADR-0182); this
//! capability never derives one from the statement it was handed.

use std::collections::BTreeMap;

use aether_actor::runtime;
use aether_bloomery::{AuthorityDoor, Digest, Ed25519KeyProvider, KeyId, Statement};
use aether_data::wire::from_bytes;
use ed25519_dalek::VerifyingKey;

use super::SigningCapability;
use super::kinds::{Verify, VerifyResult};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for [`SigningCapability`]: the real verifier over the
/// host-custodied allowlist.
pub struct SigningCapabilityState {
    provider: Ed25519KeyProvider,
}

impl SigningCapabilityState {
    /// Build state over an explicit provider — the seam the handler tests drive
    /// (they assert real verify / reject behavior against a known allowlist).
    #[must_use]
    pub fn new(provider: Ed25519KeyProvider) -> Self {
        Self { provider }
    }

    /// Decode the wire-encoded [`Statement`] and the caller-supplied
    /// `(AuthorityDoor, Digest)` authority, and verify the author signature
    /// against the allowlist as authority for exactly that door and binding
    /// (ADR-0182).
    ///
    /// Either side failing to decode is a [`VerifyResult::Err`] — an
    /// undecodable authority answers exactly as an undecodable statement does,
    /// because in both cases there is no request to check the signature against.
    /// A well-formed request that does not verify (non-author provenance,
    /// unknown / mismatched / malformed signature, or a genuine signature
    /// presented under a door or binding it was not signed for) is
    /// `Ok { verified: false }` — the fail-closed answer the gate turns into a
    /// `400`.
    #[must_use]
    pub fn verify(&self, statement: &[u8], authority: &[u8]) -> VerifyResult {
        let statement: Statement = match from_bytes(statement) {
            Ok(statement) => statement,
            Err(error) => return VerifyResult::Err { error: error.to_string() },
        };
        let (door, binding): (AuthorityDoor, Digest) = match from_bytes(authority) {
            Ok(authority) => authority,
            Err(error) => return VerifyResult::Err { error: error.to_string() },
        };
        VerifyResult::Ok { verified: statement.verify_authority(&self.provider, door, binding) }
    }
}

/// Parse the `key-id:hex-public-key` allowlist config into the provider's
/// allowlist. Each comma-separated entry pairs a [`KeyId`] with a 32-byte
/// ed25519 verifying key (64 hex chars); whitespace around entries is trimmed
/// and empty entries are skipped, so a trailing comma is tolerated. A malformed
/// entry (missing separator, bad hex, wrong length, non-canonical key point) or
/// a duplicate key-id is an error — the caller fails the boot rather than
/// trusting a smaller set or silently resolving a duplicate to one key.
///
/// # Errors
///
/// Returns a human-readable reason naming the offending entry.
pub fn parse_allowlist(allowlist: Option<&str>) -> Result<BTreeMap<KeyId, VerifyingKey>, String> {
    let mut parsed = BTreeMap::new();
    let Some(allowlist) = allowlist else {
        return Ok(parsed);
    };
    for entry in allowlist.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (id, hex) =
            entry.split_once(':').ok_or_else(|| format!("allowlist entry `{entry}` is not `key-id:hex-public-key`"))?;
        let bytes = aether_bloomery::decode_hex(hex.trim())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or_else(|| format!("allowlist entry `{id}` has a malformed hex key"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|error| format!("allowlist entry `{id}` is not a valid ed25519 key: {error}"))?;
        // A duplicate key-id is ambiguous — silently keeping the last would let a
        // later entry override an earlier signer's key without notice, a silent
        // trust change. Fail the boot instead.
        if parsed.insert(KeyId(id.trim().to_owned()), key).is_some() {
            return Err(format!("allowlist entry `{}` duplicates an earlier key-id", id.trim()));
        }
    }
    Ok(parsed)
}

#[runtime]
impl NativeActor for SigningCapability {
    type State = SigningCapabilityState;
    type Config = super::SigningConfig;

    const NAMESPACE: &'static str = "aether.signing";

    fn init(config: super::SigningConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<SigningCapabilityState, BootError> {
        let allowlist = parse_allowlist(config.allowlist.as_deref()).map_err(|error| BootError::Other(error.into()))?;
        tracing::info!(
            target: "aether_chassis_bloomery::signing",
            signers = allowlist.len(),
            "signing capability mounted"
        );
        Ok(SigningCapabilityState { provider: Ed25519KeyProvider::new(allowlist) })
    }

    // The `#[handler::single]` contract requires the mail by value; the handler
    // only borrows its field to decode, so clippy sees a by-ref opportunity the
    // macro signature cannot take.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_verify(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Verify) -> VerifyResult {
        state.verify(&mail.statement, &mail.authority)
    }
}
