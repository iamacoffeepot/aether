//! Resolved audio synth configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to `AudioCapability::init`.

/// Resolved configuration for the audio synth. Chassis mains read
/// env vars (`AETHER_AUDIO_DISABLE`, `AETHER_AUDIO_SAMPLE_RATE`)
/// into an `AudioConfig` and pass it to `with_actor::<AudioCapability>(cfg)`
/// (issue 464). Tests build an `AudioConfig` directly.
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264): the
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `AudioConfigLayer`, the clap-shaped `AudioOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims. `requested_sample_rate`'s type
/// `Option<u32>` triggers the macro's type-driven
/// `Option<numeric>` shape: the Layer holds `Option<String>` and
/// `from_layer` does the soft `.parse().ok()` so an unparseable
/// value lands as `None` (indistinguishable from unset, matching
/// the prior reader).
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_AUDIO", cli_prefix = "audio")]
pub struct AudioConfig {
    /// `AETHER_AUDIO_DISABLE=1` skips cpal init entirely. The cap
    /// still claims its mailbox and replies `Err` to `SetMasterGain`
    /// so agents fail fast instead of hanging. `env` + `cli_long`
    /// overrides pin the historical wire shape (no `D` suffix on
    /// `DISABLE`; `--audio-disable` not `--audio-disabled`).
    #[config(
        env = "AETHER_AUDIO_DISABLE",
        cli_long = "audio-disable",
        default = false
    )]
    pub disabled: bool,
    /// `AETHER_AUDIO_SAMPLE_RATE=<hz>` requests a specific rate. If
    /// the device doesn't support it, boot falls back to nop
    /// (ADR-0039 — non-fatal). `layer_field = "sample_rate"` drops
    /// the `requested_` prefix on the Layer / env / CLI side so the
    /// historical names are unchanged.
    #[config(layer_field = "sample_rate", env = "AETHER_AUDIO_SAMPLE_RATE")]
    pub requested_sample_rate: Option<u32>,
}
