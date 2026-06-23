use super::{DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS};
use std::time::Duration;

/// Resolved configuration for the `aether.anthropic` cap. Chassis
/// mains read env (`ANTHROPIC_API_KEY`, `AETHER_ANTHROPIC_DISABLE`,
/// `AETHER_ANTHROPIC_MAX_IN_FLIGHT`, `AETHER_ANTHROPIC_TIMEOUT_MS`)
/// into this and pass it to `with_actor::<AnthropicCapability>(cfg)`.
/// Tests build it directly so they never read process env.
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264): the
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `AnthropicConfigLayer`, the clap-shaped `AnthropicOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` shims under
/// `feature = "native"`. The wasm-marker build carries only the
/// domain struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "native", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "native",
    config(env_prefix = "AETHER_ANTHROPIC", cli_prefix = "anthropic")
)]
pub struct AnthropicConfig {
    /// The Messages-API key. `None` (or `disabled`) wires the
    /// `DisabledAnthropicAdapter` so Messages requests reply
    /// `Unauthorized` while the CLI path still works. `env`
    /// override pins the unprefixed `ANTHROPIC_API_KEY` key.
    #[cfg_attr(feature = "native", config(env = "ANTHROPIC_API_KEY"))]
    pub api_key: Option<String>,
    /// `AETHER_ANTHROPIC_DISABLE=1` forces the disabled adapter
    /// even when a key is present. `env` + `cli_long` overrides
    /// pin the historical wire shape (no `D` suffix on `DISABLE`).
    #[cfg_attr(
        feature = "native",
        config(
            env = "AETHER_ANTHROPIC_DISABLE",
            cli_long = "anthropic-disable",
            default = false
        )
    )]
    pub disabled: bool,
    /// Per-cap concurrency bound (doubles as rate-limit throttling).
    /// The `nonzero` hint coerces a resolved `0` (a zero-concurrency
    /// provider deadlocks) back to the default.
    #[cfg_attr(feature = "native", config(default = 2, nonzero))]
    pub max_in_flight: usize,
    /// Per-request timeout for the Messages API. The derive's
    /// `ms_duration` hint + `layer_field = "timeout_ms"` pin the
    /// Layer / env / CLI shape to the pre-derive name
    /// (`AETHER_ANTHROPIC_TIMEOUT_MS`, `--anthropic-timeout-ms`).
    #[cfg_attr(
        feature = "native",
        config(default = 120_000, ms_duration, layer_field = "timeout_ms")
    )]
    pub timeout: Duration,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            disabled: false,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{
        AnthropicConfig, AnthropicConfigLayer, DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS,
    };
    use confique::Config as _;
    use std::time::Duration;

    // ADR-0090: byte-identical to the prior hand-rolled reader,
    // exercised without touching process env (issue 464).

    #[test]
    fn anthropic_from_env_defaults_match() {
        // No `.env()` source: loads literal defaults only — env-free,
        // guards the layer defaults against the consts + default().
        let layer = AnthropicConfigLayer::builder()
            .load()
            .expect("defaults load");
        let default = AnthropicConfig::default();
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
