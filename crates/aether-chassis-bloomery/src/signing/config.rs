//! The `signing` capability's boot configuration (ADR-0090 derive-`Config`,
//! ADR-0150).
//!
//! The authorized-signer allowlist is host-local: it names which signers may
//! sign in a person's stead, the ed25519 public key each signs with, and the
//! highest approval tier each is authorized at (ADR-0151 key policy, #5324).
//! Resolved argv > env > default, it never leaves the machine it is configured
//! on (ADR-0150). An unset allowlist resolves empty, so an unconfigured
//! instance verifies nothing — the gate is fail-closed by default.

/// Where the `signing` capability's authorized-signer allowlist comes from,
/// resolved argv > env > default.
///
/// `allowlist` is a comma-separated list of `key-id:hex-public-key:tier`
/// entries, each pairing a [`KeyId`](aether_bloomery::KeyId) with the 32-byte
/// ed25519 verifying key (64 hex chars) that signer signs with and the highest
/// [`Tier`](aether_bloomery::Tier) — `auto`, `judge`, or `human` — that signer
/// is authorized to approve at (#5324). A bare `Option<String>` (not a literal
/// default) so an unset value resolves to no authorized signers at `init`;
/// `--signing-allowlist` / `AETHER_SIGNING_ALLOWLIST` override it. The runtime
/// parses the string into the [`Ed25519KeyProvider`]'s allowlist at boot,
/// failing the boot on a malformed entry rather than silently trusting a
/// smaller set.
///
/// The two-field `key-id:hex-public-key` form still parses, and resolves to the
/// `auto` ceiling — the bottom of the ladder. An entry that states no authority
/// is not an entry that states unlimited authority, and the migration is the
/// operator writing down, per key, what that key was always implicitly being
/// trusted with. The boot logs a warning naming each such signer, so a silent
/// downgrade is not how an operator discovers it.
///
/// [`Ed25519KeyProvider`]: aether_bloomery::Ed25519KeyProvider
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SIGNING", cli_prefix = "signing")]
pub struct SigningConfig {
    /// The `key-id:hex-public-key` authorized-signer entries; unset → no
    /// authorized signers (fail-closed).
    #[config(env = "AETHER_SIGNING_ALLOWLIST")]
    pub allowlist: Option<String>,
}
