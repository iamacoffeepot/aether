//! The `signing` capability's boot configuration (ADR-0090 derive-`Config`,
//! ADR-0150).
//!
//! The authorized-signer allowlist is host-local: it names which signers may
//! sign in a person's stead and the ed25519 public key each signs with
//! (ADR-0151 key policy). Resolved argv > env > default, it never leaves the
//! machine it is configured on (ADR-0150). An unset allowlist resolves empty, so
//! an unconfigured instance verifies nothing — the gate is fail-closed by
//! default.

/// Where the `signing` capability's authorized-signer allowlist comes from,
/// resolved argv > env > default.
///
/// `allowlist` is a comma-separated list of `key-id:hex-public-key` entries,
/// each pairing a [`KeyId`](aether_bloomery::KeyId) with the 32-byte ed25519
/// verifying key (64 hex chars) that signer signs with. A bare `Option<String>`
/// (not a literal default) so an unset value resolves to no authorized signers
/// at `init`; `--signing-allowlist` / `AETHER_SIGNING_ALLOWLIST` override it.
/// The runtime parses the string into the [`Ed25519KeyProvider`]'s allowlist at
/// boot, failing the boot on a malformed entry rather than silently trusting a
/// smaller set.
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
