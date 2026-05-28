//! Full-surface accept: a copy of `HttpConfig`'s field shape, proving
//! the derive handles `bool`, `HashSet<String>` (csv_set), `usize`,
//! and `Duration` (ms_duration) in one go.

use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;

pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;

#[allow(clippy::unnecessary_wraps)]
fn parse_flag(s: &str) -> Result<bool, Infallible> {
    Ok(s == "1" || s.eq_ignore_ascii_case("true"))
}

#[allow(clippy::unnecessary_wraps)]
fn parse_allowlist(s: &str) -> Result<HashSet<String>, Infallible> {
    Ok(s.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect())
}

#[allow(clippy::unnecessary_wraps)]
fn parse_max_body_bytes(s: &str) -> Result<usize, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_MAX_BODY_BYTES))
}

#[allow(clippy::unnecessary_wraps)]
fn parse_timeout_ms(s: &str) -> Result<u32, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_TIMEOUT_MS))
}

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_HTTP", cli_prefix = "http")]
pub struct HttpConfig {
    #[config(default = false, parse = parse_flag)]
    pub disabled: bool,
    #[config(default = [], parse = parse_allowlist, csv_set)]
    pub allowlist: HashSet<String>,
    #[config(default = false, parse = parse_flag)]
    pub require_https: bool,
    #[config(default = 16_777_216, parse = parse_max_body_bytes)]
    pub max_body_bytes: usize,
    #[config(default = 30_000, parse = parse_timeout_ms, ms_duration)]
    pub default_timeout: Duration,
}

fn main() {}
