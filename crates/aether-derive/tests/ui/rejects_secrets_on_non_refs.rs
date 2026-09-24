//! `secrets` is only valid on a `SecretRefs` field — on a `String` the resolve
//! step would have nothing to bind, so it is caught at expansion time rather
//! than silently accepted.

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_BAD", cli_prefix = "bad")]
pub struct BadConfig {
    #[config(secrets)]
    pub api_key: String,
}

fn main() {}
