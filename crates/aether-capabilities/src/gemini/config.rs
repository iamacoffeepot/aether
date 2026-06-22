use super::{DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS};
// confique consumes these through `#[config(parse_env = …)]`; IntelliJ-Rust
// doesn't trace macro-attr path args (Qodana FP), but rustc + clippy do.
#[allow(unused_imports)]
use crate::config_env::{parse_flag, parse_millis_strict, parse_provider_max_in_flight_strict};
use std::time::Duration;

/// Resolved configuration for the `aether.gemini` cap. Chassis mains
/// read env (`GEMINI_API_KEY`, `AETHER_GEMINI_DISABLE`,
/// `AETHER_GEMINI_MAX_IN_FLIGHT`, `AETHER_GEMINI_TIMEOUT_MS`); the
/// staging root reads `AETHER_GEN_DIR` at stage time (shared with
/// issue 1013 / 1014). Tests build it directly.
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264): the
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `GeminiConfigLayer`, the clap-shaped `GeminiOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` shims under
/// `feature = "native"`. The wasm-marker build carries only the
/// domain struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "native", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "native",
    config(env_prefix = "AETHER_GEMINI", cli_prefix = "gemini")
)]
pub struct GeminiConfig {
    /// The Google API key. `None` (or `disabled`) wires the
    /// `DisabledGeminiAdapter`. `env` override pins the unprefixed
    /// `GEMINI_API_KEY` key.
    #[cfg_attr(feature = "native", config(env = "GEMINI_API_KEY"))]
    pub api_key: Option<String>,
    /// `AETHER_GEMINI_DISABLE=1` forces the disabled adapter.
    /// `env` + `cli_long` overrides pin the historical wire shape
    /// (no `D` suffix on `DISABLE`).
    #[cfg_attr(
        feature = "native",
        config(
            env = "AETHER_GEMINI_DISABLE",
            cli_long = "gemini-disable",
            default = false,
            parse = parse_flag
        )
    )]
    pub disabled: bool,
    /// Per-cap concurrency bound (doubles as rate-limit throttling).
    #[cfg_attr(
        feature = "native",
        config(default = 2, parse = parse_provider_max_in_flight_strict)
    )]
    pub max_in_flight: usize,
    /// Per-request timeout for the media APIs. The derive's
    /// `ms_duration` hint + `layer_field = "timeout_ms"` pin the
    /// Layer / env / CLI shape to the pre-derive name
    /// (`AETHER_GEMINI_TIMEOUT_MS`, `--gemini-timeout-ms`).
    #[cfg_attr(
        feature = "native",
        config(
            default = 180_000,
            parse = parse_millis_strict::<DEFAULT_TIMEOUT_MILLIS>,
            ms_duration,
            layer_field = "timeout_ms"
        )
    )]
    pub timeout: Duration,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            disabled: false,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
        }
    }
}

// confique's `parse_env` contract is `fn(&str) -> Result<T, impl Error>`,
// so these total helpers carry a `Result` they never fill with `Err`.
// The strict (erroring) variants land with the ADR-0090 §4 validation
// pass; hence the per-fn `unnecessary_wraps` allow.

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS, GeminiConfig, GeminiConfigLayer};
    use confique::Config as _;
    use std::time::Duration;

    // ADR-0090: the confique migration is byte-identical to the prior
    // hand-rolled reader. These exercise the resolution logic without
    // touching process env (issue 464).

    #[test]
    fn gemini_from_env_defaults_match() {
        // No `.env()` source: loads literal defaults only, so this is
        // env-free and guards the layer's defaults against the named
        // consts + `GeminiConfig::default()`.
        let layer = GeminiConfigLayer::builder().load().expect("defaults load");
        let default = GeminiConfig::default();
        assert_eq!(layer.api_key, None);
        assert!(!layer.disabled);
        assert_eq!(layer.max_in_flight, DEFAULT_MAX_IN_FLIGHT);
        assert_eq!(layer.timeout_ms, DEFAULT_TIMEOUT_MILLIS);
        assert_eq!(
            Duration::from_millis(u64::from(layer.timeout_ms)),
            default.timeout
        );
    }
}
