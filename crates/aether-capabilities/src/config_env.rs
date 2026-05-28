//! Shared confique `parse_env` helpers used by the capabilities'
//! `*Layer` structs (ADR-0090). Centralized here so each per-cap
//! migration doesn't carry its own copy of the identical bool /
//! `> 0`-filtered concurrency parsers (Qodana `DuplicatedCode`).
//!
//! Every helper is total — `Result<T, Infallible>` because confique's
//! `parse_env` contract mandates the wrapper, even when the parse
//! cannot fail. The strict (erroring) variants land with the
//! ADR-0090 §4 validation pass (e1).

use std::convert::Infallible;

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

/// Provider concurrency bound: positive integer, otherwise fall back
/// to [`DEFAULT_PROVIDER_MAX_IN_FLIGHT`]. The `> 0` filter rejects
/// `AETHER_*_MAX_IN_FLIGHT=0` (which would otherwise deadlock the cap
/// by allowing no in-flight requests). Soft-fall-back today;
/// ADR-0090 §4's hard-error lands in e1.
#[allow(clippy::unnecessary_wraps)]
pub fn parse_provider_max_in_flight(s: &str) -> Result<usize, Infallible> {
    Ok(s.parse::<usize>()
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_PROVIDER_MAX_IN_FLIGHT))
}

/// Generic ms-soft-parse: positive `u32` from the env string, fall back
/// to `DEFAULT_MS` (the per-cap default literal) on parse failure. Each
/// caller instantiates with its own turbofish (e.g.
/// `parse_env = parse_u32_ms_or::<DEFAULT_GEMINI_TIMEOUT_MS>`), so the
/// expanded bodies are textually distinct — what Qodana's
/// `DuplicatedCode` looks for. Soft-fall-back today; ADR-0090 §4's
/// hard-error lands in e1.
#[allow(clippy::unnecessary_wraps)]
pub fn parse_u32_ms_or<const DEFAULT_MS: u32>(s: &str) -> Result<u32, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_MS))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_PROVIDER_MAX_IN_FLIGHT, parse_flag, parse_provider_max_in_flight, parse_u32_ms_or,
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
    fn parse_provider_max_in_flight_filters_non_positive_and_unparseable() {
        assert_eq!(parse_provider_max_in_flight("4").unwrap(), 4);
        assert_eq!(
            parse_provider_max_in_flight("0").unwrap(),
            DEFAULT_PROVIDER_MAX_IN_FLIGHT
        );
        assert_eq!(
            parse_provider_max_in_flight("garbage").unwrap(),
            DEFAULT_PROVIDER_MAX_IN_FLIGHT
        );
        assert_eq!(
            parse_provider_max_in_flight("").unwrap(),
            DEFAULT_PROVIDER_MAX_IN_FLIGHT
        );
    }

    #[test]
    fn parse_u32_ms_or_soft_falls_back_to_const_generic() {
        assert_eq!(parse_u32_ms_or::<30_000>("5000").unwrap(), 5000);
        assert_eq!(parse_u32_ms_or::<30_000>("garbage").unwrap(), 30_000);
        assert_eq!(parse_u32_ms_or::<30_000>("").unwrap(), 30_000);
        // Different turbofish → different defaults — what makes the
        // per-cap call sites textually distinct under DuplicatedCode.
        assert_eq!(parse_u32_ms_or::<120_000>("garbage").unwrap(), 120_000);
    }
}
