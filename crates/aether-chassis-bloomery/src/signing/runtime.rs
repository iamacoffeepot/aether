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
use aether_bloomery::{
    AuthorityDoor, AuthorizedSigner, Digest, Ed25519KeyProvider, KeyId, KeyProvider, Statement, Tier,
};
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
    ///
    /// A `required_tier` asks the allowlist's *second* question once the
    /// signature has held: is this signer authorized this high (#5324). A
    /// ceiling below it answers [`VerifyResult::BelowTier`] naming both tiers,
    /// never `Ok { verified: true }` — this is the only place the two halves of
    /// the key policy meet, because it is the only place that holds the keys. A
    /// verified signature whose signer has no ceiling at all cannot happen
    /// through this provider (verification reads the same allowlist row), and
    /// is refused as unverified rather than assumed authorized.
    #[must_use]
    pub fn verify(&self, statement: &[u8], authority: &[u8], required_tier: Option<Tier>) -> VerifyResult {
        let statement: Statement = match from_bytes(statement) {
            Ok(statement) => statement,
            Err(error) => return VerifyResult::Err { error: error.to_string() },
        };
        let (door, binding): (AuthorityDoor, Digest) = match from_bytes(authority) {
            Ok(authority) => authority,
            Err(error) => return VerifyResult::Err { error: error.to_string() },
        };
        if !statement.verify_authority(&self.provider, door, binding) {
            return VerifyResult::Ok { verified: false };
        }
        let Some(required) = required_tier else {
            return VerifyResult::Ok { verified: true };
        };
        let Some(ceiling) = statement.author_signer().and_then(|signer| self.provider.tier_ceiling(signer)) else {
            return VerifyResult::Ok { verified: false };
        };
        if ceiling < required {
            return VerifyResult::BelowTier { required, ceiling };
        }
        VerifyResult::Ok { verified: true }
    }
}

/// The tier an allowlist entry that states none is authorized at (#5324).
///
/// The bottom of the ladder, because an entry that states no authority is not
/// an entry that states unlimited authority. A key at this ceiling signs
/// nothing the gate needs a signature for — an `auto` surface forms its own
/// approval — so the old two-field form keeps parsing while authorizing
/// exactly what it wrote down, which is nothing.
const UNSTATED_CEILING: Tier = Tier::Auto;

/// Parse the `key-id:hex-public-key:tier` allowlist config into the provider's
/// allowlist. Each comma-separated entry pairs a [`KeyId`] with a 32-byte
/// ed25519 verifying key (64 hex chars) and the highest [`Tier`] that signer may
/// approve at; whitespace around entries is trimmed and empty entries are
/// skipped, so a trailing comma is tolerated. The two-field
/// `key-id:hex-public-key` form still parses and resolves to
/// [`UNSTATED_CEILING`], with a warning per entry. A malformed entry (missing
/// separator, bad hex, wrong length, non-canonical key point, unknown tier
/// spelling) or a duplicate key-id is an error — the caller fails the boot
/// rather than trusting a smaller set, silently resolving a duplicate to one
/// key, or guessing what an unreadable tier meant.
///
/// # Errors
///
/// Returns a human-readable reason naming the offending entry.
pub fn parse_allowlist(allowlist: Option<&str>) -> Result<BTreeMap<KeyId, AuthorizedSigner>, String> {
    let mut parsed = BTreeMap::new();
    let Some(allowlist) = allowlist else {
        return Ok(parsed);
    };
    for entry in allowlist.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (id, rest) = entry
            .split_once(':')
            .ok_or_else(|| format!("allowlist entry `{entry}` is not `key-id:hex-public-key:tier`"))?;
        let id = id.trim();

        // The key is fixed-width hex with no separator of its own, so the last
        // colon — when there is a second one — is the tier's.
        let (hex, ceiling) = if let Some((hex, tier)) = rest.rsplit_once(':') {
            (hex, parse_tier(tier.trim()).ok_or_else(|| unknown_tier(id, tier.trim()))?)
        } else {
            tracing::warn!(
                target: "aether_chassis_bloomery::signing",
                signer = %id,
                ceiling = ?UNSTATED_CEILING,
                "allowlist entry states no tier; authorizing it at the bottom of the ladder"
            );
            (rest, UNSTATED_CEILING)
        };
        let bytes = aether_bloomery::decode_hex(hex.trim())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or_else(|| format!("allowlist entry `{id}` has a malformed hex key"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|error| format!("allowlist entry `{id}` is not a valid ed25519 key: {error}"))?;
        // A duplicate key-id is ambiguous — silently keeping the last would let a
        // later entry override an earlier signer's key without notice, a silent
        // trust change. Fail the boot instead.
        if parsed.insert(KeyId(id.to_owned()), AuthorizedSigner { key, ceiling }).is_some() {
            return Err(format!("allowlist entry `{id}` duplicates an earlier key-id"));
        }
    }
    Ok(parsed)
}

/// The policy-text spelling of a tier ceiling, or `None` outside the ladder.
///
/// Spelled out here rather than deserialized through serde's aliases so the
/// accepted set is one readable list at the boundary that reads it, and so an
/// unknown spelling is a boot error naming what the operator wrote rather than
/// a deserializer's message about a type they never mentioned.
fn parse_tier(tier: &str) -> Option<Tier> {
    match tier {
        "auto" => Some(Tier::Auto),
        "judge" => Some(Tier::Judge),
        "human" => Some(Tier::Human),
        _ => None,
    }
}

fn unknown_tier(id: &str, tier: &str) -> String {
    format!("allowlist entry `{id}` names tier `{tier}`; expected one of auto, judge, human")
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
        state.verify(&mail.statement, &mail.authority, mail.required_tier)
    }
}
