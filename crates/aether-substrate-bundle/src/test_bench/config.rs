//! Configuration for the test-bench chassis (ADR-0090).

use super::bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH};

/// Render-size knob for the standalone test-bench binary
/// (`AETHER_TEST_BENCH_SIZE=WxH`). Mirrors the single-field
/// `SettlementConfig` shape:
/// a `#[derive(aether_substrate::Config)]` struct resolved `from_env()`
/// and lowered to `(u32, u32)` by [`Self::to_size`].
///
/// The explicit `env =` pin is belt-and-suspenders against a future field
/// rename, matching how `ActorRingConfig` pins its historical keys.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_TEST_BENCH", cli_prefix = "test-bench")]
pub struct RenderSizeConfig {
    /// `AETHER_TEST_BENCH_SIZE=WxH` render dimensions for the offscreen
    /// wgpu surface. Falls back to `800x600` on missing/unparseable input
    /// with a warn log (default `None`).
    #[config(env = "AETHER_TEST_BENCH_SIZE")]
    pub size: Option<String>,
}

impl RenderSizeConfig {
    /// Lower the resolved knob to `(width, height)` pixels. Preserves the
    /// `parse_size_env` semantics verbatim: missing env var, missing `x`
    /// separator, non-numeric parts, or a zero dimension all fall back to
    /// [`DEFAULT_WIDTH`] × [`DEFAULT_HEIGHT`] with a `warn` log.
    #[must_use]
    pub fn to_size(&self) -> (u32, u32) {
        let Some(raw) = self.size.as_deref() else {
            return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        };
        if let Some((w, h)) = raw.split_once('x') {
            match (w.parse::<u32>(), h.parse::<u32>()) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
                _ => {
                    tracing::warn!(
                        target: "aether_substrate::boot",
                        value = %raw,
                        "AETHER_TEST_BENCH_SIZE unparseable — falling back to default",
                    );
                    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
                }
            }
        } else {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_TEST_BENCH_SIZE missing 'x' separator — falling back to default",
            );
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: Option<&str>) -> RenderSizeConfig {
        RenderSizeConfig {
            size: s.map(str::to_owned),
        }
    }

    #[test]
    fn to_size_valid() {
        assert_eq!(cfg(Some("640x480")).to_size(), (640, 480));
    }

    #[test]
    fn to_size_none_falls_back() {
        assert_eq!(cfg(None).to_size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn to_size_no_separator_falls_back() {
        // Tripwire: missing 'x' separator must fall back (not panic/error).
        assert_eq!(cfg(Some("640")).to_size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn to_size_non_numeric_falls_back() {
        // Tripwire: non-numeric WxH parts must fall back.
        assert_eq!(cfg(Some("axb")).to_size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn to_size_zero_dimension_falls_back() {
        // Tripwire: a zero width or height must fall back (not produce a
        // zero-sized surface that crashes the GPU init).
        assert_eq!(
            cfg(Some("0x480")).to_size(),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        );
    }
}
