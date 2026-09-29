//! Resolved audio synth configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to `AudioCapability::init`.

/// Resolved configuration for the audio synth. Chassis mains read
/// env vars (`AETHER_AUDIO_OUTPUT`, `AETHER_AUDIO_SAMPLE_RATE`)
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
    /// Where the synth's samples go: `device`, `null` (the synth runs and its samples are discarded), or `disabled`.
    ///
    /// `disabled` skips the synth entirely: the cap still claims its
    /// mailbox and replies `Err` to its requests so agents fail fast
    /// instead of hanging. `null` runs the real synth thread and event
    /// queue without opening a device, so every request behaves as on a
    /// desktop with audio.
    #[config(default = "device")]
    pub output: AudioOutput,
    /// Requested output sample rate in hertz; unset uses the device default.
    ///
    /// If the device doesn't support the requested rate, boot falls back
    /// to nop (ADR-0039 — non-fatal). A `null` output runs at this rate,
    /// or at 48 kHz when unset. `layer_field = "sample_rate"` drops
    /// the `requested_` prefix on the Layer / env / CLI side so the
    /// historical names are unchanged.
    #[config(layer_field = "sample_rate", env = "AETHER_AUDIO_SAMPLE_RATE")]
    pub requested_sample_rate: Option<u32>,
}

/// Where the synth's samples go ([`AudioConfig::output`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AudioOutput {
    /// The default output device, through cpal.
    #[default]
    Device,
    /// No device: the synth runs on its own thread and its samples are
    /// discarded.
    Null,
    /// No synth: every request that needs one replies `Err`.
    Disabled,
}
