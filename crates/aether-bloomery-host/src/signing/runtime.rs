//! The `Ed25519KeyProvider`-backed runtime for [`SigningCapability`] (ADR-0149
//! step 3, ADR-0150).
//!
//! State holds the real verifier over the host-local authorized-signer
//! allowlist. `init` parses the `key-id:hex-public-key` config into the
//! provider, failing the boot on a malformed entry rather than silently
//! trusting a smaller set (a dropped signer would read as "not authorized" — a
//! silent security downgrade). The single `#[handler::single] on_verify`
//! delegates to the inherent [`verify`](SigningCapabilityState::verify), which
//! decodes the wire `Statement` and runs the pure
//! [`Statement::verify_authority`](aether_bloomery::Statement::verify_authority)
//! against the provider — so the "only author signatures verify" rule stays in
//! core and the test drives the exact method the handler does.

use std::collections::BTreeMap;

use aether_actor::runtime;
use aether_bloomery::{Ed25519KeyProvider, KeyId, Statement};
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

    /// Decode the wire-encoded [`Statement`] and verify its author signature
    /// against the allowlist. A statement that does not decode is a
    /// [`VerifyResult::Err`]; a well-formed one that does not verify (non-author
    /// provenance, unknown / mismatched / malformed signature) is
    /// `Ok { verified: false }` — the fail-closed answer the gate turns into a
    /// `400`.
    #[must_use]
    pub fn verify(&self, statement: &[u8]) -> VerifyResult {
        let statement: Statement = match from_bytes(statement) {
            Ok(statement) => statement,
            Err(error) => return VerifyResult::Err { error: error.to_string() },
        };
        VerifyResult::Ok { verified: statement.verify_authority(&self.provider) }
    }
}

/// Parse the `key-id:hex-public-key` allowlist config into the provider's
/// allowlist. Each comma-separated entry pairs a [`KeyId`] with a 32-byte
/// ed25519 verifying key (64 hex chars); whitespace around entries is trimmed
/// and empty entries are skipped, so a trailing comma is tolerated. A malformed
/// entry (missing separator, bad hex, wrong length, non-canonical key point) is
/// an error — the caller fails the boot rather than trusting a smaller set.
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
        let bytes =
            decode_key_hex(hex.trim()).ok_or_else(|| format!("allowlist entry `{id}` has a malformed hex key"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|error| format!("allowlist entry `{id}` is not a valid ed25519 key: {error}"))?;
        parsed.insert(KeyId(id.trim().to_owned()), key);
    }
    Ok(parsed)
}

/// Decode a 64-char hex string into the 32 raw bytes of an ed25519 public key,
/// or `None` on a wrong length or a non-hex character.
fn decode_key_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let high = hex_nibble(hex.as_bytes()[index * 2])?;
        let low = hex_nibble(hex.as_bytes()[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(bytes)
}

/// One hex digit (0-9, a-f, A-F) → its nibble value, or `None`.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[runtime]
impl NativeActor for SigningCapability {
    type State = SigningCapabilityState;
    type Config = super::SigningConfig;

    const NAMESPACE: &'static str = "aether.signing";

    fn init(config: super::SigningConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<SigningCapabilityState, BootError> {
        let allowlist = parse_allowlist(config.allowlist.as_deref()).map_err(|error| BootError::Other(error.into()))?;
        tracing::info!(
            target: "aether_bloomery_host::signing",
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
        state.verify(&mail.statement)
    }
}
