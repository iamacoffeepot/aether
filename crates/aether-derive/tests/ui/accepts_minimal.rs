//! Minimal accept case: one numeric field, container with both
//! prefixes, a `default` + `parse` pass-through. Proves the
//! happy-path Layer + Overlay + impls compile and the `parse_env`
//! turbofish-free shape works.

use std::convert::Infallible;

#[allow(clippy::unnecessary_wraps)]
fn identity_fn(s: &str) -> Result<u32, Infallible> {
    Ok(s.parse().unwrap_or(0))
}

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_MIN", cli_prefix = "min")]
pub struct MinimalConfig {
    #[config(default = 0, parse = identity_fn)]
    pub value: u32,
}

fn main() {}
