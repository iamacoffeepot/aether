//! Init config for the input subscription cap (ADR-0090).

/// Configuration for [`InputCapability`](super::InputCapability). Empty
/// today — the cap builds its subscriber table from scratch and reaches
/// for `Mailer` / `Registry` through `NativeInitCtx`. Kept as a struct
/// so the chassis composes the cap with the same
/// `Builder::with_actor::<InputCapability>(InputConfig {})` shape
/// as every other cap and a future config knob (e.g. ring caps,
/// per-stream gates) lands without API churn.
#[derive(Default)]
pub struct InputConfig {}
