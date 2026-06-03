//! Shared confique `parse_env` helpers used by the capabilities'
//! `*Layer` structs (ADR-0090). Centralized here so each per-cap
//! migration doesn't carry its own copy of the identical bool /
//! `> 0`-filtered concurrency parsers (Qodana `DuplicatedCode`).
//!
//! Two parser families live here. [`parse_flag`] is total —
//! `Result<bool, Infallible>` — because a bool reader can't fail (any
//! non-truthy string is `false`). The **strict** (erroring) numeric
//! variants are the ADR-0090 §4 hard-error half (unit e1): a malformed
//! *known* env value returns `Err`, which confique surfaces from
//! `.load()` and the chassis env resolver converts into a
//! `ConfigError`. An empty value is treated as unset (falls back to
//! the default) — only a non-empty unparseable value aborts boot.

use std::convert::Infallible;
use std::num::ParseIntError;

/// Default per-cap concurrency bound shared by the content-gen
/// providers (`aether.gemini`, `aether.anthropic`) when their
/// `AETHER_*_MAX_IN_FLIGHT` env var is unset, non-positive, or
/// unparseable. ADR-0050.
pub const DEFAULT_PROVIDER_MAX_IN_FLIGHT: usize = 2;

/// `"1"` or `"true"` (case-insensitive) → `true`, anything else
/// `false`, matching the prior hand-rolled flag readers across
/// http / gemini / anthropic / audio.
#[allow(clippy::unnecessary_wraps)]
pub fn parse_flag(s: &str) -> Result<bool, Infallible> {
    Ok(s == "1" || s.eq_ignore_ascii_case("true"))
}

/// Strict ms parser (ADR-0090 §4 hard-error half): a non-empty value
/// that doesn't parse as `u32` returns `Err`, which confique surfaces
/// from `.load()` and the chassis env resolver converts into a
/// `ConfigError`. An *empty* string is treated as unset and falls back
/// to `DEFAULT_MILLIS` (an env var set to `""` is not a typo to abort on).
///
/// # Errors
///
/// Returns the underlying [`ParseIntError`] when a non-empty value is
/// not a valid `u32`.
pub fn parse_millis_strict<const DEFAULT_MILLIS: u32>(s: &str) -> Result<u32, ParseIntError> {
    if s.trim().is_empty() {
        return Ok(DEFAULT_MILLIS);
    }
    s.trim().parse()
}

/// Strict provider concurrency bound (ADR-0090 §4 hard-error half).
/// Empty → [`DEFAULT_PROVIDER_MAX_IN_FLIGHT`] (unset); a non-empty
/// value that doesn't parse as `usize` errors; a parsed `0` clamps up
/// to the default (a zero-concurrency provider deadlocks, so it is
/// treated as "use the default", not a hard error — matching the soft
/// variant's `> 0` filter).
///
/// # Errors
///
/// Returns the underlying [`ParseIntError`] when a non-empty value is
/// not a valid `usize`.
pub fn parse_provider_max_in_flight_strict(s: &str) -> Result<usize, ParseIntError> {
    if s.trim().is_empty() {
        return Ok(DEFAULT_PROVIDER_MAX_IN_FLIGHT);
    }
    let n: usize = s.trim().parse()?;
    Ok(if n > 0 {
        n
    } else {
        DEFAULT_PROVIDER_MAX_IN_FLIGHT
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_PROVIDER_MAX_IN_FLIGHT, parse_flag, parse_millis_strict,
        parse_provider_max_in_flight_strict,
    };

    #[test]
    fn parse_flag_matches_legacy_bool_reader() {
        for truthy in ["1", "true", "TRUE", "True"] {
            assert!(parse_flag(truthy).unwrap(), "{truthy} should be truthy");
        }
        for falsy in ["0", "", "yes", "false", "garbage"] {
            assert!(!parse_flag(falsy).unwrap(), "{falsy} should be falsy");
        }
    }

    #[test]
    fn parse_provider_max_in_flight_strict_errors_on_garbage() {
        // ADR-0090 §4: a parseable value passes; `0` clamps to the
        // default (zero-concurrency deadlocks); empty is unset; a
        // non-empty non-number errors rather than silently defaulting.
        assert_eq!(parse_provider_max_in_flight_strict("4"), Ok(4));
        assert_eq!(
            parse_provider_max_in_flight_strict("0"),
            Ok(DEFAULT_PROVIDER_MAX_IN_FLIGHT)
        );
        assert_eq!(
            parse_provider_max_in_flight_strict(""),
            Ok(DEFAULT_PROVIDER_MAX_IN_FLIGHT)
        );
        assert!(parse_provider_max_in_flight_strict("garbage").is_err());
    }

    #[test]
    fn parse_millis_strict_errors_on_garbage() {
        assert_eq!(parse_millis_strict::<30_000>("5000"), Ok(5000));
        assert_eq!(parse_millis_strict::<30_000>(""), Ok(30_000));
        assert!(parse_millis_strict::<30_000>("garbage").is_err());
        // Different turbofish → different defaults (the unset path),
        // keeping the per-cap call sites textually distinct.
        assert_eq!(parse_millis_strict::<120_000>(""), Ok(120_000));
    }
}
