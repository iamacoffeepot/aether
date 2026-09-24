//! `probe` bundle — the entry `Probe` fixture plus the ADR-0090 c1
//! `ProbeWithConfig` typed-config fixture, exported together via
//! `export!(Probe, ProbeWithConfig)` (ADR-0096, issue 1994).
//!
//! # `Probe`
//!
//! Test-fixture component for substrate-feature scenarios. Not a
//! demo, not exemplary — its only job is to expose substrate /
//! substrate-harness primitives (window-event subscription, drop, replace, capture)
//! to scenario assertions in a way that's easy to observe.
//!
//! Behaviour:
//!
//! - On every tick, sends `aether.test_fixture.tick_observed` to the
//!   substrate-harness observer mailbox (`aether.substrate_harness.observer`) with
//!   a monotonic counter. Lets scenarios count tick deliveries via
//!   `SubstrateHarness::count_observed` (issue 775 retired the
//!   `BroadcastCapability` MCP fan-out; the harness now owns a private
//!   catch-all observer mailbox for these scenario observations).
//! - Reports every `Key` and `TextInput` it receives to the same observer,
//!   and drops its `Key` subscription on `UnsubscribeKeys`.
//!
//! It declares the observer it reports to, and only the `SubstrateHarness`
//! registers that observer, so `Probe` loads only there. The render
//! behaviour lives on `PaintProbe` and the asset-window pull and first-tick
//! log on `QuietProbe`, each its own actor that declares what it mails
//! (ADR-0232 §6).
//!
//! ADR-0090 c1: this fixture moved from `aether-test-fixture-probe`'s
//! `src/lib.rs` to `aether-test-fixtures-bundle/src/probe.rs`; the shared
//! `TickObserved` kinds moved to the sibling lib so integration tests can
//! import them without reaching into a cdylib.
//!
//! # `ProbeWithConfig`
//!
//! ADR-0090 c1 typed-config fixture. Exercises the
//! `WasmActor::Config = ProbeConfig` path end-to-end: the host places
//! wire-encoded `ProbeConfig` bytes in a delivery region (ADR-0095) during
//! `Component::instantiate`; the guest's `init_with_config_p32` shim decodes
//! them via `<ProbeConfig as Kind>::decode_from_bytes` and threads
//! the typed struct into `Probe::init(config, ctx)`.
//!
//! The fixture stashes `(seed, label)` at boot and replies with a
//! `ConfigEcho` on every `ConfigQuery` mail so a test can assert the
//! config round-tripped intact. No tick behaviour — the sibling
//! `Probe` covers that.
//!
//! Consumers load it from the `probe` bundle stem with
//! `export: Some("test.probe_with_config")` (ADR-0096).

// `on_key` only re-broadcasts the inbound payload, so it doesn't touch
// `self`; it keeps `&mut self` to match the `#[handler]` dispatch ABI.
// `ProbeWithConfig::on_config_query` takes `&mut self` for the same reason.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, Erased, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Key, TextInput, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_test_fixtures_kinds::{
    ConfigEcho, ConfigQuery, KeyObserved, ProbeConfig, SubstrateHarnessObserver, TextInputObserved, TickObserved,
    UnsubscribeKeys,
};
use aether_window::WindowCapability;

pub struct Probe {
    tick_count: u64,
}

#[actor(depends(LifecycleCapability, WindowCapability, SubstrateHarnessObserver))]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe { tick_count: 0 })
    }

    /// Issue 640: explicit subscribe in `wire`; init can't mail (its ctx
    /// has no send surface, issue 703).
    ///
    /// `Tick` is a frame-lifecycle stage, so it subscribes on
    /// `aether.lifecycle` (ADR-0082). `Key` and `TextInput` originate at
    /// windows, so the probe subscribes to every window through
    /// `aether.window` (ADR-0164).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_, Self>) {
        ctx.subscribe::<LifecycleCapability, Tick>();
        ctx.subscribe::<WindowCapability, Key>();
        ctx.subscribe::<WindowCapability, TextInput>();
    }

    /// Counts ticks delivered to this mailbox; broadcasts the running
    /// total so scenarios can observe it on the loopback.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once
    /// per advance for every lifecycle-subscribed mailbox. Watch
    /// `receive_mail` for `aether.test_fixture.tick_observed` to see
    /// the count climbing.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _: Tick) {
        self.tick_count += 1;
        ctx.send::<SubstrateHarnessObserver>(&TickObserved { count: self.tick_count });
    }

    /// Broadcasts a `key_observed` for each `Key` dispatch, so the
    /// ADR-0164 window round-trip scenarios can count selector-aware
    /// fan-out deliveries (subscribe / unsubscribe / drop-clears).
    ///
    /// # Agent
    /// Not sent manually; the window actor's fan-out fires it for
    /// every matching subscriber when a key is pressed.
    /// Watch `receive_mail` for `aether.test_fixture.key_observed`.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        ctx.send::<SubstrateHarnessObserver>(&KeyObserved { code: key.code });
    }

    /// Unsubscribe this probe from `Key` on every window, the self-addressed
    /// counterpart of the `wire` subscribe.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.unsubscribe_keys` to the probe; later key
    /// presses stop producing `key_observed` from it.
    #[handler::single]
    fn on_unsubscribe_keys(&mut self, ctx: &mut WasmCtx<'_, Self>, _: UnsubscribeKeys) {
        ctx.unsubscribe::<WindowCapability, Key>();
    }

    /// Broadcasts a `text_input_observed` for each `TextInput` dispatch,
    /// so the ADR-0164 round-trip scenario can assert the window actor
    /// fanned the committed-text stream out to a subscriber.
    ///
    /// # Agent
    /// Not sent manually; the window actor's fan-out fires it for
    /// every `TextInput`-subscribed mailbox when text is committed.
    /// Watch `receive_mail` for `aether.test_fixture.text_input_observed`.
    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        ctx.send::<SubstrateHarnessObserver>(&TextInputObserved { text: input.text });
    }
}

/// ADR-0090 c1 typed-config fixture. Exercises the
/// `WasmActor::Config = ProbeConfig` path end-to-end.
///
/// Consumers load this actor from the `probe` bundle with
/// `export: Some("test.probe_with_config")`.
pub struct ProbeWithConfig {
    seed: u32,
    label: String,
}

#[actor]
impl WasmActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "test.probe_with_config";

    fn init(config: ProbeConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ProbeWithConfig { seed: config.seed, label: config.label })
    }

    /// Reply with a `ConfigEcho` describing the cached config. Lets
    /// the integration test observe what the typed `init` actually
    /// received without scraping logs or readback.
    #[handler::manual]
    fn on_config_query(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _query: ConfigQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&ConfigEcho { seed: self.seed, label: self.label.clone() });
        }
    }
}
