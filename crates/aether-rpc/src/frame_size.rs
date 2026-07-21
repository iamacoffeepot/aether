//! The wire-frame size knob as an operator-resolvable config member
//! (ADR-0156 §6).
//!
//! `AETHER_MAX_FRAME_SIZE` used to be pull-read inside `aether-codec` on the
//! first framing call, and shadowed by a hand `KnobRecord` in `aether-chassis`
//! so the unknown-key sweep tolerated it — `aether-codec` sits below the
//! actor/config system and cannot resolve config, which left the knob an
//! orphan of the composition-derived aggregate.
//!
//! [`FrameSizeConfig`] gives it a home: a `#[derive(aether_substrate::Config)]`
//! member that resolves through the normal source stack (argv > env > file >
//! default) and lowers to the byte cap each chassis and `aether-mcp` pushes
//! set-once into the codec ([`aether_codec::frame::install_max_frame_size`])
//! at boot, before any framing runs. It lives in `aether-rpc` because that is
//! the one wire crate every framing consumer already shares — the full-stack
//! chassis (transitively) and `aether-mcp` (directly) — and it depends on both
//! `aether-substrate` (the derive machinery) and `aether-codec` (the default
//! and the install seam).

use aether_codec::frame::MAX_FRAME_SIZE;

/// The compiled frame-cap default this member resolves to when unset. The
/// confique `default` literal below cannot reference a const, so this assertion
/// is the compile-time guard that pins the literal to the codec's own default
/// — the drift check the retired `KnobRecord` sync-note used to ask for by
/// hand.
const _: () = assert!(
    MAX_FRAME_SIZE == 67_108_864,
    "the AETHER_MAX_FRAME_SIZE config default literal must equal aether_codec::frame::MAX_FRAME_SIZE",
);

/// Operator-resolvable maximum wire-frame body size (ADR-0156 §6). Resolved
/// once at boot through the source stack and pushed into `aether-codec` via
/// [`Self::to_max_frame_size`] + [`aether_codec::frame::install_max_frame_size`].
///
/// The `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `FrameSizeConfigLayer`, the clap-shaped `FrameSizeOverlay`, the
/// `FromArgvThenEnv` impl, the inherent `from_env` / `from_argv_then_env`
/// shims, and the `ConfigMember` declaration (section `[frame]`) the chassis
/// aggregate walks.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER", cli_prefix = "frame")]
pub struct FrameSizeConfig {
    /// Maximum accepted wire-frame body size in bytes.
    ///
    /// Unset resolves to the codec's compiled default (64 MiB); the codec
    /// clamps the installed value to its 1 GiB ceiling so a runaway
    /// override cannot defeat the OOM guard.
    #[config(env = "AETHER_MAX_FRAME_SIZE", default = 67_108_864)]
    pub max_frame_size: usize,
}

impl Default for FrameSizeConfig {
    fn default() -> Self {
        Self { max_frame_size: MAX_FRAME_SIZE }
    }
}

impl FrameSizeConfig {
    /// Lower the resolved knob to the byte cap the codec installs. A resolved
    /// `0` reproduces the historical "unset/garbage → default" rather than
    /// installing a 0-byte cap that would reject every frame; the codec clamps
    /// the ceiling on install.
    #[must_use]
    pub fn to_max_frame_size(&self) -> usize {
        if self.max_frame_size == 0 {
            MAX_FRAME_SIZE
        } else {
            self.max_frame_size
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the `0 → default` coercion is the only logic this type owns
    /// over the derive-resolved value. Drifts if the coercion is dropped (a
    /// resolved 0 would then install a frame-rejecting 0-byte cap) or the
    /// default stops tracking the codec.
    #[test]
    fn to_max_frame_size_coerces_zero_to_default() {
        assert_eq!(FrameSizeConfig { max_frame_size: 0 }.to_max_frame_size(), MAX_FRAME_SIZE);
        assert_eq!(FrameSizeConfig { max_frame_size: 4096 }.to_max_frame_size(), 4096);
    }
}
