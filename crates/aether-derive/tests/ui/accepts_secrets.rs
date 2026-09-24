//! Accept case for the `secrets` hint (ADR-0235): a `SecretRefs` field gets the
//! secret-refs `parse_env`, an empty default, a `FromStr`-parsed overlay slot,
//! and a `ConfigMember::resolve` that binds the refs to the source stack's
//! secrets directory. Proves the whole emission compiles against the real
//! `aether_substrate::config` surface.

use aether_substrate::config::SecretRefs;

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_VAULTED", cli_prefix = "vaulted")]
pub struct VaultedConfig {
    #[config(default = false)]
    pub disabled: bool,
    /// Names only; the values live in the secrets directory.
    #[config(secrets)]
    pub secrets: SecretRefs,
}

fn main() {}
