//! Container attribute `env_prefix` is mandatory. Omitting it (here
//! the container only declares `cli_prefix`) is a deterministic
//! compile-error.

#[derive(aether_derive::Config)]
#[config(cli_prefix = "missing")]
pub struct MissingPrefixConfig {
    pub value: u32,
}

fn main() {}
