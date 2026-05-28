//! `ms_duration` hint: domain field `Duration`, Layer field `<n>_ms: u32`.
//! The macro emits the `Duration::from_millis(u64::from(layer.<n>_ms))`
//! bridge in `from_layer` automatically.

use std::convert::Infallible;
use std::time::Duration;

pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;

#[allow(clippy::unnecessary_wraps)]
fn parse_timeout_ms(s: &str) -> Result<u32, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_TIMEOUT_MS))
}

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_MS", cli_prefix = "ms")]
pub struct MsDurationConfig {
    #[config(default = 30_000, parse = parse_timeout_ms, ms_duration)]
    pub timeout: Duration,
}

fn main() {}
