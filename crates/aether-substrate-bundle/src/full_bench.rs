//! Transitional full-composition bridge for the bundle-resident bench
//! tests (issue #3765). Composes the pre-#3764 cap set in one call so
//! the ~40 scenario tests still living in this crate (and aether-kit's,
//! until #3768) keep their historical composition; a test rehoming to
//! its cap crate states its minimal composition instead and drops this.

use aether_clipboard::{ClipboardCapability, ClipboardConfig};
use aether_game::{GameGatewayCapability, GameGatewayConfig};
use aether_input::{InputCapability, InputConfig};
use aether_substrate_bench::SubstrateBenchBuilder;
use aether_substrate_bench_capture::RenderBenchBuilderExt;
use aether_tcp::TcpCapability;
use aether_text::TextCapability;

/// Compose the full pre-#3764 cap set: render + component host + input
/// + tcp + text + in-memory clipboard + inert game gateway (fs still
/// rides `namespace_roots`).
pub trait FullBenchExt {
    #[must_use]
    fn full(self) -> Self;

    /// [`Self::full`] without the game gateway — for the loopback game
    /// scenarios that compose an active gateway config of their own
    /// (two gateways would contend for the `aether.game` mailbox).
    #[must_use]
    fn full_sans_game(self) -> Self;
}

impl FullBenchExt for SubstrateBenchBuilder {
    fn full(self) -> Self {
        self.full_sans_game().with_actor::<GameGatewayCapability>(GameGatewayConfig::default())
    }

    fn full_sans_game(self) -> Self {
        self.with_render()
            .with_component_host()
            .with_actor::<InputCapability>(InputConfig::default())
            .with_actor::<TcpCapability>(())
            .with_actor::<TextCapability>(())
            .with_actor::<ClipboardCapability>(ClipboardConfig::InMemory)
    }
}
