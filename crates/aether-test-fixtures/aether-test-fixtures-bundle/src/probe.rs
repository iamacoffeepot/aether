//! `probe` bundle — the entry `Probe` fixture plus the ADR-0090 c1
//! `ProbeWithConfig` typed-config fixture, exported together via
//! `export!(Probe, ProbeWithConfig)` (ADR-0096, issue 1994).
//!
//! # `Probe`
//!
//! Test-fixture component for substrate-feature scenarios. Not a
//! demo, not exemplary — its only job is to expose substrate /
//! substrate-harness primitives (input subscription, drop, replace, capture)
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
//! - On the first tick, emits a `tracing::info!("typed_send_alive")`
//!   that flows through the actor-aware subscriber (issue #581) →
//!   per-actor `LogBuffer` → drain at handler exit ships a `LogBatch`
//!   to the `aether.log` mailbox. Pre-#581 this fixture exercised the
//!   issue-563 stage-5 typed-sender path against `LogEvent`; #581
//!   demoted `LogEvent` to a non-mailable struct so the buffer-and-
//!   drain shape is the only sender path for log content.
//! - Receives `aether.test_fixture.set_render { r, g, b, visible }`
//!   to update render state. When `visible` is non-zero, `on_tick`
//!   emits a colored `DrawTriangle` to the chassis render sink, so
//!   `capture_frame` scenarios can observe pre-mail effects in the
//!   captured PNG.
//!
//! ADR-0090 c1: this fixture moved from `aether-test-fixture-probe`'s
//! `src/lib.rs` to `aether-test-fixtures/aether-test-fixtures-bundle/src/probe.rs`. The
//! actor source is unchanged; the shared `TickObserved` / `SetRender`
//! kinds moved to the sibling lib so integration tests can import
//! them without reaching into a cdylib.
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
//! config round-tripped intact. No tick / render behaviour — the
//! sibling `Probe` covers that.
//!
//! Consumers load it from the `probe` bundle stem with
//! `export: Some("test.probe_with_config")` (ADR-0096).

// `on_key` only re-broadcasts the inbound payload, so it doesn't touch
// `self`; it keeps `&mut self` to match the `#[handler]` dispatch ABI.
// `ProbeWithConfig::on_config_query` takes `&mut self` for the same reason.
#![allow(clippy::unused_self)]

use aether_actor::{
    ActorInitError, AssetWindow, MailSender, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_input::{InputCapability, InputMailboxExt};
use aether_kinds::{Key, TextInput, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::Rgb;
use aether_render::{DrawTriangle, RenderCapability, Vertex};
use aether_test_fixtures_kinds::{
    AssetProbe, AssetProbeResult, ConfigEcho, ConfigQuery, KeyObserved, ProbeConfig,
    SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, SetRender, TextInputObserved, TickObserved,
};

pub struct Probe {
    tick_count: u64,
    render: SetRender,
    /// ADR-0163 §3 (#3984): what `wire` pulled from the asset load window,
    /// surfaced later through [`Probe::on_asset_probe`].
    asset: AssetProbeResult,
}

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe { tick_count: 0, render: SetRender::default(), asset: AssetProbeResult::default() })
    }

    //noinspection DuplicatedCode
    /// Issue 640: explicit subscribe in `wire`; init can't mail (its ctx
    /// has no send surface, issue 703).
    ///
    /// `Tick` is a frame-lifecycle stage, so it subscribes on
    /// `aether.lifecycle` (ADR-0082); `Key` is a genuine input interrupt,
    /// so it subscribes on `aether.input` (ADR-0021) — the input-stream
    /// path the round-trip scenarios exercise (issue 1490).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        ctx.actor::<InputCapability>().subscribe::<Key>();
        ctx.actor::<InputCapability>().subscribe::<TextInput>();
        // ADR-0163 §3 (#3984): pull the bundle's asset through the load
        // window (open during `wire`) and stash a content fingerprint —
        // length + a wrapping-sum checksum — so a later `AssetProbe` proves
        // the guest-side pull round-tripped the exact bytes across the FFI
        // and that the value survived the window closing after `wire`.
        if let Some(bytes) = ctx.asset("asset_fixture.txt") {
            let checksum = bytes.iter().fold(0u64, |acc, &byte| acc.wrapping_add(u64::from(byte)));
            self.asset = AssetProbeResult { pulled: true, len: bytes.len() as u64, checksum };
        }
    }

    /// Counts ticks delivered to this mailbox; broadcasts the running
    /// total so scenarios can observe it on the loopback. When the
    /// stored render state is `visible`, also emits a colored
    /// `DrawTriangle` covering most of the frame so `capture_frame`
    /// scenarios can see the pre-mail effect in the PNG.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once
    /// per advance for every input-subscribed mailbox. Watch
    /// `receive_mail` for `aether.test_fixture.tick_observed` to see
    /// the count climbing.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _: Tick) {
        self.tick_count += 1;
        ctx.send_to_named::<TickObserved>(
            SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
            &TickObserved { count: self.tick_count },
        );
        if self.tick_count == 1 {
            tracing::info!(target: "aether_test_fixture_probe", "typed_send_alive");
        }
        if self.render.visible != 0 {
            let r = f32::from(self.render.r) / 255.0;
            let g = f32::from(self.render.g) / 255.0;
            let b = f32::from(self.render.b) / 255.0;
            let v = |x: f32, y: f32| Vertex { x, y, z: 0.5, color: Rgb::new(r, g, b) };
            ctx.actor::<RenderCapability>().send(&DrawTriangle { verts: [v(-0.9, -0.9), v(0.9, -0.9), v(0.0, 0.9)] });
        }
    }

    /// Broadcasts a `key_observed` for each `Key` input dispatch, so the
    /// ADR-0021 input round-trip scenarios can count `aether.input`
    /// fan-out deliveries (subscribe / unsubscribe / drop-clears) on a
    /// genuine input interrupt.
    ///
    /// # Agent
    /// Not sent manually; the substrate's input fan-out fires it for
    /// every `aether.input`-subscribed mailbox when a key is pressed.
    /// Watch `receive_mail` for `aether.test_fixture.key_observed`.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        ctx.send_to_named::<KeyObserved>(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, &KeyObserved { code: key.code });
    }

    /// Broadcasts a `text_input_observed` for each `TextInput` dispatch,
    /// so the ADR-0021 round-trip scenario can assert the `aether.input`
    /// cap fanned the committed-text stream out to a subscriber.
    ///
    /// # Agent
    /// Not sent manually; the substrate's input fan-out fires it for
    /// every `TextInput`-subscribed mailbox when text is committed.
    /// Watch `receive_mail` for `aether.test_fixture.text_input_observed`.
    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        ctx.send_to_named::<TextInputObserved>(
            SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
            &TextInputObserved { text: input.text },
        );
    }

    /// Updates the stored render state. Subsequent ticks paint the
    /// new color (or stop painting when `visible == 0`).
    ///
    /// # Agent
    /// Send via `send_mail` with `kind_name = "aether.test_fixture.set_render"`
    /// and params `{ r, g, b, visible }`. Used by `capture_frame`
    /// scenarios to flip the fixture's render output between frames.
    #[handler::single]
    fn on_set_render(&mut self, _ctx: &mut WasmCtx<'_>, mail: SetRender) {
        self.render = mail;
    }

    /// ADR-0163 §3 (#3984): reply with the fingerprint of the asset this
    /// fixture pulled from its load window during `wire`. Runs post-`wire`
    /// (the window has closed), so a non-zero `pulled` reply proves the
    /// guest-side `AssetWindow::asset` pull worked while the window was open
    /// and the bytes survived into the instance's ordinary state.
    ///
    /// # Agent
    /// Send `aether.test_fixtures.asset_probe`; the reply
    /// `aether.test_fixtures.asset_probe_result` carries `{ pulled, len,
    /// checksum }`.
    #[handler::manual]
    fn on_asset_probe(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: AssetProbe) {
        if ctx.reply_target().is_some() {
            ctx.reply(&self.asset);
        }
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
    fn on_config_query(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: ConfigQuery) {
        if ctx.reply_target().is_some() {
            ctx.reply(&ConfigEcho { seed: self.seed, label: self.label.clone() });
        }
    }
}
