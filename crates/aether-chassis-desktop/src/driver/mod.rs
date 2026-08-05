//! Desktop chassis driver capability — ADR-0071 / ADR-0161 / ADR-0164.
//!
//! The driver is a pure pump host: it boots two pumped slots on the winit
//! thread — `aether.window` and `aether.render` — via
//! [`DriverCtx::boot_pumped_actor`] from a Claim-stage reservation, pushing
//! them mail, and draining. [`DesktopWindowApplication`] owns all winit
//! callbacks and native-window state; [`DesktopRenderIntegration`] supplies
//! only render, lifecycle-settlement, and graceful-shutdown semantics.
//!
//! `DesktopDriverRunning::run` blocks on `event_loop.run_app(&mut app)`, runs
//! each pumped slot's `shutdown` teardown on exit, and emits the shutdown
//! telemetry. Returning means the user closed the window or the event loop
//! exited cleanly; the `chassis_builder` then tears down every passive in
//! reverse boot order via `BootedPassives::Drop`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_data::Kind;
use aether_data::{encode_empty, mailbox_id_from_name};
use aether_kinds::{Quit, Tick, WindowId as EngineWindowId};
use aether_render::{Frame, Occluded, RenderCapability, RenderCapabilityState, RenderParams, RenderTuningConfig};
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::settlement::{PumpWake, TerminalDisposition, WaitOutcome, await_settlement_pumped};
use aether_substrate::config::{ConfigMember, ConfigMemberRecord};
use aether_substrate::runtime::lifecycle as runtime_lifecycle;
use aether_substrate::{
    ChassisCtx, HubOutbound, Mailer, SettlingInbox, Source, SourceAddr, SubstrateBoot,
    chassis::frame_loop,
    mail::{Mail, MailId, MailboxId},
};
use aether_window::{
    DesktopWindowApplication, DesktopWindowCapability, DesktopWindowIntegration, DesktopWindowParams,
    INITIAL_WINDOW_NAME, WindowSizeRequest, WindowSpec,
};
use crossbeam_channel::{Receiver, Sender};
use winit::event_loop::EventLoop;
use winit::window::Window;

use super::chassis::UserEvent;
use lifecycle::{LifecycleReplyOutcome, consume_lifecycle_reply};
use shutdown::install_shutdown_handler;

mod lifecycle;
mod shutdown;

/// Cumulative patience cap for the per-frame settlement gates (advance +
/// capture pre-mail). The per-round budget is `frame_loop::DRAIN_BUDGET`
/// (the log cadence); a starved-but-healthy chain resolves before this
/// cap, a genuine wedge exhausts it (issue #1305).
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

/// Chassis-owned semantic integration for the window application's render,
/// lifecycle-settlement, and graceful-shutdown operations.
pub struct DesktopRenderIntegration {
    queue: Arc<Mailer>,
    /// `aether.lifecycle` mailbox id, cached at boot. Each redraw
    /// fires one `LifecycleAdvance` here; the cap broadcasts the `Tick`
    /// stage directly to its stage subscribers (issue 1490 retired the
    /// `Tick → aether.input` relay; components subscribe `Tick` on
    /// `aether.lifecycle`), then the driver waits for settlement before
    /// submitting the frame.
    lifecycle_mailbox: MailboxId,
    kind_lifecycle_advance: aether_data::KindId,
    /// `aether.lifecycle.advance_reply` inbox claimed at boot (issue
    /// 1378). The per-frame `Tick → Render` cycle pushes each
    /// `LifecycleAdvance` with this mailbox as its `Component` reply
    /// target, then synchronously drains the receiver for the cap's
    /// `LifecycleAdvanceComplete` reply. The reply is emitted only after
    /// the cap clears its pending-advance guard (ADR-0082 §6), so gating
    /// the next advance on it — rather than on the raw settlement channel
    /// — keeps the back-to-back advances from racing the cap's overlap
    /// guard (the same reply-gate the substrate-harness frame loop uses,
    /// iamacoffeepot/aether#999).
    lifecycle_reply_inbox: SettlingInbox,
    /// Mailbox id of [`Self::lifecycle_reply_inbox`], used as the
    /// `Component` reply target stamped onto each `LifecycleAdvance`.
    lifecycle_reply_mailbox: MailboxId,
    /// Hub outbound — held for log egress to the hub and
    /// `lifecycle::fatal_abort`. NOT used for chassis replies:
    /// `HubOutbound::send_reply` only routes `Session` / `EngineMailbox`
    /// targets and silently drops `SourceAddr::Component`, but mail
    /// dispatched by this engine's own `RpcServerCapability` (every
    /// hub/MCP call lands via the proxy → local RPC server) carries a
    /// `Component(rpc_server)` reply target. Replies go through the
    /// `Mailer` instead (`mail.reply` for the chain-joined window arms,
    /// `req.reply.reply` through the retained inbound guard for the
    /// deferred capture replies, #1758), which pushes the reply as local
    /// mail so the RPC server's `on_any` lifts it into a `ReplyEvent`
    /// (iamacoffeepot/aether#1316).
    outbound: Arc<HubOutbound>,
    /// The pumped `aether.render` runtime (ADR-0161 §Decision 1), dispatched
    /// on this winit thread through a [`PumpedSlot`] — the same pump home the
    /// `aether.window` actor uses. It owns the wgpu surfaces, the accumulators,
    /// and the pending capture outright; the driver only pushes it
    /// [`Frame`] / [`Occluded`] mail and drains. Booted from the driver's
    /// Claim-stage `aether.render` reservation via [`DriverCtx::boot_pumped_actor`].
    render_slot: PumpedSlot<RenderCapability>,
    /// `aether.render` mailbox id, cached at boot — the recipient for the
    /// per-frame [`Frame`] request and the [`Occluded`] forward.
    render_mailbox: MailboxId,
    /// Unified [`PumpWake`] channel (ADR-0161 §Decision 2). The render slot's
    /// mailbox wake sends [`PumpWake::Mail`] here and each per-advance
    /// settlement subscription sends [`PumpWake::Settled`];
    /// [`await_settlement_pumped`] selects across both from the one channel
    /// std `mpsc` could not. `render_pump_tx` is cloned into the per-advance
    /// `subscribe_settlement_with` callback; `render_pump_rx` is the wait's
    /// read end.
    render_pump_tx: Sender<PumpWake>,
    render_pump_rx: Receiver<PumpWake>,
    started: Option<Instant>,
    /// Start instant of the previous Tick stage. The desktop cadence is
    /// frame-driven, so this is the source of Tick's elapsed-time payload.
    last_tick: Option<Instant>,
    frame: u64,
    /// ADR-0080 §6 chassis-root correlation counter (issue
    /// iamacoffeepot/aether#723). Bumped per chassis-source push so
    /// every chassis-owned render/lifecycle emission carries a fresh
    /// `MailId` for the trace observer to root a tree on. Symmetric
    /// with the per-actor counter on `NativeBinding`.
    chassis_correlation: AtomicU64,
    /// True once graceful lifecycle shutdown has been requested.
    quit_requested: bool,
    /// Set after the lifecycle reaches its `Shutdown` terminal.
    terminal_reached: bool,
}

impl DesktopRenderIntegration {
    /// ADR-0080 §6 chassis-source push helper (issue
    /// iamacoffeepot/aether#723). Mints a fresh correlation, calls
    /// `push_chassis_root_mail` so the trace observer sees a `Sent`
    /// event for every chassis-owned render/lifecycle emission. Returns the
    /// minted chain-root [`MailId`] so frame-gating callers can
    /// subscribe its settlement (ADR-0082 §6).
    fn push_chassis_root(
        &self,
        recipient: MailboxId,
        kind: aether_data::KindId,
        payload: Vec<u8>,
        count: u32,
    ) -> MailId {
        let mut correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        if correlation == 0 {
            correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        }
        self.queue.push_chassis_root_mail(correlation, recipient, kind, payload, count)
    }

    /// Begin graceful shutdown exactly once. The window application drives a
    /// frame immediately after this request and exits only after the lifecycle
    /// reports its terminal.
    fn request_quit(&mut self) {
        if self.quit_requested {
            return;
        }
        self.quit_requested = true;
        self.push_chassis_root(self.lifecycle_mailbox, <Quit as Kind>::ID, encode_empty::<Quit>(), 1);
    }

    /// Mint a chassis-root `LifecycleAdvance` and push it to the
    /// `aether.lifecycle` cap with [`Self::lifecycle_reply_mailbox`] as
    /// its `Component` reply target (issue 1378). Open-codes the
    /// chassis-root push (`push_chassis_root_mail` doesn't carry a reply
    /// target): mint id → record `Sent` for the trace subtree → push with
    /// both the chassis-root lineage and the reply-to. Returns the minted
    /// chain root so the caller can subscribe its settlement.
    fn push_lifecycle_advance(&self, delta_micros: u32) -> MailId {
        let mut correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        if correlation == 0 {
            correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        }
        let advance_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, correlation);
        self.queue.record_sent(
            advance_root,
            advance_root,
            None,
            MailboxId::CHASSIS_MAILBOX_ID,
            self.lifecycle_mailbox,
            self.kind_lifecycle_advance,
        );
        let reply_to = Source::with_correlation(SourceAddr::Component(self.lifecycle_reply_mailbox), correlation);
        self.queue.push(
            Mail::new(
                self.lifecycle_mailbox,
                self.kind_lifecycle_advance,
                aether_kinds::LifecycleAdvance { delta_micros }.encode_into_bytes(),
                1,
            )
            .with_lineage(advance_root, advance_root, None)
            .with_reply_to(reply_to),
        );
        advance_root
    }

    /// Block (bounded) for the `LifecycleAdvanceComplete` reply to the
    /// just-issued `LifecycleAdvance`, returning its `next` stage kind id
    /// (issue 1378). The reply lands on [`Self::lifecycle_reply_inbox`]
    /// and is emitted by the cap *after* it clears its pending-advance
    /// guard, so the caller can safely issue the next advance once this
    /// returns. The settlement wait the caller runs first guarantees the
    /// reply is imminent; the generous timeout is a wedge backstop, not
    /// the normal path. `None` on timeout (no reply after settlement) so
    /// the caller can fail-fast.
    fn recv_lifecycle_advance_next(&self) -> Option<u64> {
        loop {
            let mail = self.lifecycle_reply_inbox.recv_timeout(FRAME_SETTLEMENT_CAP)?;
            // ADR-0106: the consumed reply settles when its `InboundMail`
            // guard drops inside `consume_lifecycle_reply` — no hand-rolled
            // bracket. Pre-#1704 a dropped armed guard aborted the process
            // on every painted frame.
            match consume_lifecycle_reply(mail) {
                LifecycleReplyOutcome::Complete(next) => return next,
                // Unexpected kind on this dedicated reply inbox (nothing
                // else targets it); already settled — keep waiting for the
                // advance reply rather than mis-gating the cycle.
                LifecycleReplyOutcome::Unexpected => {}
            }
        }
    }

    /// Push a chassis-internal render mail (a per-frame [`Frame`] request or
    /// an [`Occluded`] forward) to the pumped `aether.render` mailbox and
    /// drain the slot so its handler runs inline on this thread. Draining
    /// here — rather than only on the mailbox wake — is what lets an
    /// [`Occluded`] forward fail-fast a parked capture the moment the window
    /// hides, and lets a [`Frame`] request record + present within the
    /// redraw that issued it.
    fn send_render_and_drain<K: Kind>(&mut self, mail: &K) {
        self.push_chassis_root(self.render_mailbox, K::ID, mail.encode_into_bytes(), 1);
        self.render_slot.drain_available();
    }

    /// Drive one full `Tick → Render → Present` lifecycle cycle (ADR-0082
    /// §11), returning `true` when the cycle reached the `Shutdown` terminal.
    /// Each `LifecycleAdvance` broadcasts the cap's current stage; components
    /// emit their `DrawTriangle` / `aether.view_projection` mail into the
    /// pumped render mailbox as descendants of that advance's chain root.
    ///
    /// The settlement wait is [`await_settlement_pumped`], not the pooled
    /// `await_internal_signal` (ADR-0161 §Decision 2): the very draw mail the
    /// chain is gated on lands on this driver's own pumped render slot, so a
    /// wait that could not pump the slot would deadlock on the first frame.
    /// Each advance subscribes its root through the callback-form
    /// `subscribe_settlement_with`, which sends [`PumpWake::Settled`] into the
    /// unified channel the slot's mailbox wake also feeds — so the wait wakes
    /// on both a settled chain and mail arrival, draining the slot on the
    /// latter. Reading `LifecycleAdvanceComplete.next` (not the raw
    /// settlement) gates the next advance on the cap clearing its
    /// pending-advance guard (iamacoffeepot/aether#999).
    fn run_frame_advance(&mut self, delta_micros: u32) -> bool {
        loop {
            let advance_root = self.push_lifecycle_advance(delta_micros);
            if let WaitOutcome::Wedged(wedge) = self.pump_while_settling(advance_root) {
                runtime_lifecycle::fatal_abort(&self.outbound, wedge.reason());
            }
            match self.recv_lifecycle_advance_next() {
                // Terminal reached (`next == 0`): the `Shutdown` broadcast has
                // fired and settled. Present this last frame, then exit.
                Some(0) => return true,
                // Back at Tick (cycle complete) — stop and present.
                Some(next) if next == <Tick as Kind>::ID.0 => return false,
                // Mid-cycle (Tick → Render → Present) — keep advancing.
                Some(_) => {}
                // Settlement fired but the reply never arrived — a wedge in
                // the reply path; fail-fast like the settlement wait above.
                None => runtime_lifecycle::fatal_abort(
                    &self.outbound,
                    "desktop.frame_advance: LifecycleAdvanceComplete reply did not arrive after settlement".to_owned(),
                ),
            }
        }
    }

    fn metrics(&self) -> (Option<Instant>, u64, u64) {
        (self.started, self.frame, self.render_slot.read_state(RenderCapabilityState::triangles_rendered).unwrap_or(0))
    }

    fn shutdown(&mut self) {
        self.render_slot.shutdown();
    }
}

impl DesktopWindowIntegration for DesktopRenderIntegration {
    fn attach_window(&mut self, id: EngineWindowId, window: Arc<Window>) -> Result<(), String> {
        let attachment = self
            .render_slot
            .host_turn(|state, _ctx| state.attach_window(id, window))
            .ok_or_else(|| "render actor is unavailable during window attachment".to_owned())?;
        attachment?;
        let attached = Instant::now();
        self.started.get_or_insert(attached);
        self.last_tick.get_or_insert(attached);
        Ok(())
    }

    fn detach_window(&mut self, id: EngineWindowId) {
        if self.render_slot.host_turn(|state, _ctx| state.detach_window(id)) == Some(false) {
            tracing::warn!(
                target: "aether_substrate::render",
                window_id = id.0,
                "window manager detached an unknown render target",
            );
        }
    }

    fn windows_dirty(&mut self, windows: &[EngineWindowId]) {
        if self.terminal_reached {
            return;
        }
        let now = Instant::now();
        let delta_micros = self
            .last_tick
            .replace(now)
            .map_or(0, |last_tick| u32::try_from(now.duration_since(last_tick).as_micros()).unwrap_or(u32::MAX));
        self.terminal_reached = self.run_frame_advance(delta_micros);
        self.send_render_and_drain(&Frame { replay_cache_when_idle: false, windows: windows.to_vec() });
        self.frame += 1;
    }

    fn window_occluded(&mut self, id: EngineWindowId, occluded: bool) {
        self.send_render_and_drain(&Occluded { window: id, occluded });
    }

    fn request_shutdown(&mut self) {
        self.request_quit();
    }

    fn drain_available(&mut self) {
        self.render_slot.drain_available();
    }

    fn capture_deadline(&self) -> Option<Instant> {
        self.render_slot.read_state(RenderCapabilityState::capture_deadline).flatten()
    }

    fn should_exit(&self) -> bool {
        self.terminal_reached
    }

    fn pump_while_settling(&mut self, settlement: MailId) -> WaitOutcome {
        let Some(registry) = self.queue.settlement_registry().cloned() else {
            return WaitOutcome::Settled;
        };
        let pump_tx = self.render_pump_tx.clone();
        registry.subscribe_settlement_with(settlement, move || {
            let _ = pump_tx.send(PumpWake::Settled);
        });
        await_settlement_pumped(
            &self.render_pump_rx,
            &mut self.render_slot,
            "desktop.frame_advance",
            frame_loop::DRAIN_BUDGET,
            FRAME_SETTLEMENT_CAP,
            TerminalDisposition::Abort,
        )
    }
}

/// ADR-0071 driver capability for the desktop chassis. Owns the
/// pieces the winit event-loop body needs at construction time, then
/// `boot()` builds the window-owned application plus `DriverRunning`.
///
/// The substrate-core handle (`SubstrateBoot`) rides along on the
/// running so the scheduler stays alive for the chassis's lifetime.
pub struct DesktopDriverCapability {
    pub event_loop: EventLoop<UserEvent>,
    pub boot: SubstrateBoot,
    /// Lowered window boot knobs (mode / size / title / wireframe), resolved in
    /// the desktop `Chassis::build` off the source stack and threaded here as a
    /// unit. Boot passes the native-window fields to the window-owned
    /// application and the wireframe field to render.
    pub window: aether_chassis::WindowSettings,
    /// Resolved render tuning (the vertex-buffer cap) — ADR-0161 R3 boots the
    /// pumped render actor with it, so the render `Config` is resolved on the
    /// chassis's source stack and threaded here rather than through the pooled
    /// `with_actor` path that composed `RenderCapability` before the swap.
    pub render_config: RenderTuningConfig,
    /// Resolved `assets` namespace root, threaded into the pumped render
    /// actor's params so its `capture_frame` handler can read similarity
    /// reference images off the hot path (iamacoffeepot/aether#1780).
    pub assets_dir: PathBuf,
}

pub struct DesktopDriverRunning {
    app: DesktopWindowApplication<DesktopRenderIntegration>,
    event_loop: EventLoop<UserEvent>,
    /// `SubstrateBoot` drops at the end of `run()`. The `chassis_builder`
    /// `BootedPassives` (holding audio/io/http/log runnings) drops just
    /// after, tearing down each passive in reverse boot order via
    /// `RunningCapability::shutdown`. Render is no longer a passive — the
    /// pumped render slot lives on `app` and is torn down in `run()`.
    _boot: SubstrateBoot,
}

impl DriverCapability for DesktopDriverCapability {
    type Running = DesktopDriverRunning;

    /// ADR-0155 §4 / issue #3834: reserve the `aether.window` inbox at the
    /// Claim stage. The desktop driver is the cap for `aether.window` (issue
    /// 603 Phase 3), but its winit `EventLoop` does not exist at claim time —
    /// so this value-free hook only reserves the registry slot, stashing the
    /// live `MailboxClaim` for [`Self::boot`] to recover at Start (where the
    /// event loop exists) and install the `EventLoopProxy` wake. Splitting
    /// the reservation off Start is what lets `--describe` claim
    /// `aether.window` on a headless host without an event loop.
    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        // ADR-0161 R3: the desktop driver is the pump host for both the
        // `aether.window` and (post-swap) `aether.render` actors, so it
        // reserves both driver-as-actor inboxes at the Claim stage;
        // `boot_pumped_actor` recovers each at Start. `aether.render` is no
        // longer claimed by a pooled `RenderCapability` on desktop.
        ctx.claim_driver_mailbox(DesktopWindowCapability::NAMESPACE)?;
        ctx.claim_driver_mailbox("aether.render")
    }

    /// ADR-0156 §4: the window boot knobs (`AETHER_WINDOW_MODE` /
    /// `AETHER_WINDOW_TITLE` / `AETHER_WIREFRAME`) are resolved at the desktop
    /// driver seam that constructs the window application and render
    /// integration, so the chassis config aggregate carries them only where a
    /// window composes (headless, which drives a std timer, declares no window
    /// knob).
    fn config_members() -> Vec<ConfigMemberRecord> {
        // The window knobs plus the render tuning knob
        // (`AETHER_RENDER_VERTEX_BUFFER_BYTES`): ADR-0161 R3 moved render off
        // the pooled `with_actor` compose on desktop, so the driver — which
        // now boots the render actor — declares its `Config` member for the
        // manifest / `--print-config` / unknown-env sweep.
        let mut members = <aether_chassis::WindowConfig as ConfigMember>::members();
        members.extend(<RenderTuningConfig as ConfigMember>::members());
        members
    }

    // One-shot boot wiring: mailbox claims and application construction
    // thread through a single flat sequence.
    #[allow(clippy::too_many_lines)]
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { event_loop, boot, window, render_config, assets_dir } = self;
        let aether_chassis::WindowSettings { mode, size, title, wireframe } = window;
        let initial_window = WindowSpec {
            name: INITIAL_WINDOW_NAME.to_owned(),
            title,
            mode,
            size: size.map(|(width, height)| WindowSizeRequest { width, height }),
        };

        // ADR-0161: the desktop driver boots the pumped `aether.render` actor
        // itself (below). There is no cross-thread render seam — the pumped
        // actor owns the surfaces, accumulators, and pending capture as plain
        // state on this thread.

        // Issue 603 Phase 3 / ADR-0160 §Decision 3: the desktop driver is the
        // pump host for the `aether.window` actor (`DesktopWindowCapability`).
        // `boot_pumped_actor` recovers the Claim-stage `aether.window`
        // reservation `DesktopDriverCapability::claim` made (ADR-0155 §4 —
        // recovered here at Start rather than re-claiming, since a second
        // claim would collide), builds the pumped slot, runs the actor's
        // `init` / `wire`, and hands back the claim's wake slot. The manager
        // owns native window creation, identity, controls, and event routing;
        // `about_to_wait` drains it inline between frames.
        let (window_slot, window_wake_slot) =
            ctx.boot_pumped_actor::<DesktopWindowCapability>((), DesktopWindowParams)?;

        DesktopWindowApplication::<DesktopRenderIntegration>::install_wake(
            event_loop.create_proxy(),
            &window_wake_slot,
        );

        // Boot the pumped `aether.render` actor from the driver's Claim-stage
        // reservation. Native windows attach later through same-thread host
        // ingress as the window manager realizes them.
        let (render_slot, render_wake_slot) = ctx.boot_pumped_actor::<RenderCapability>(
            render_config,
            RenderParams { observed_kinds: None, assets_dir: Some(assets_dir), offscreen_size: None, wireframe },
        )?;

        // ADR-0161 §Decision 2: the unified `PumpWake` channel. The render
        // slot's mailbox wake sends `PumpWake::Mail` so the advance-loop
        // `await_settlement_pumped` drains on mail arrival, and *also* pokes
        // `UserEvent::WindowMail` so a render mail landing while the loop is
        // parked (a `capture_frame` on an occluded window) turns the loop and
        // `about_to_wait` drains the slot — the generalized "a pumped slot's
        // wake pokes a redraw" rule. `subscribe_settlement_with` sends
        // `PumpWake::Settled` into the same channel per advance.
        let (render_pump_tx, render_pump_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let render_mail_proxy = event_loop.create_proxy();
        let render_wake_tx = render_pump_tx.clone();
        render_wake_slot.set(Arc::new(move || {
            let _ = render_wake_tx.send(PumpWake::Mail);
            let _ = render_mail_proxy.send_event(UserEvent::WindowMail);
        }));

        // Chassis route-freezing: the pumped render actor's own id (its
        // NAMESPACE), the recipient for the per-frame `Frame` request and the
        // `Occluded` forward. ctx-less, no sibling resolver in scope — the
        // lifecycle route below uses the same escape hatch.
        #[allow(clippy::disallowed_methods)]
        let render_mailbox = mailbox_id_from_name(<RenderCapability as Addressable>::NAMESPACE);

        // Chassis route-freezing: the desktop driver wires its event loop to
        // the lifecycle cap's own id (its NAMESPACE) at construction time —
        // ctx-less, no sibling resolver in scope.
        #[allow(clippy::disallowed_methods)]
        let lifecycle_mailbox = mailbox_id_from_name(<aether_lifecycle::LifecycleCapability as Addressable>::NAMESPACE);
        let kind_lifecycle_advance = <aether_kinds::LifecycleAdvance as Kind>::ID;

        // The watcher sends the window-owned `Quit` event directly; the
        // application converts it to semantic graceful shutdown.
        install_shutdown_handler(event_loop.create_proxy());

        // Issue 1378: claim a dedicated inbox for the cap's
        // `LifecycleAdvanceComplete` replies. The per-frame `Tick →
        // Render` cycle stamps this as the `Component` reply target on
        // each `LifecycleAdvance` and drains the receiver synchronously
        // to gate the next advance (see `recv_lifecycle_advance_next`).
        let lifecycle_reply_claim = ctx.claim_mailbox("aether.lifecycle.advance_reply")?;

        let integration = DesktopRenderIntegration {
            queue: Arc::clone(&boot.queue),
            lifecycle_mailbox,
            kind_lifecycle_advance,
            lifecycle_reply_inbox: lifecycle_reply_claim.inbox,
            lifecycle_reply_mailbox: lifecycle_reply_claim.id,
            outbound: Arc::clone(&boot.outbound),
            render_slot,
            render_mailbox,
            render_pump_tx,
            render_pump_rx,
            started: None,
            last_tick: None,
            frame: 0,
            // 0 is the "no correlation" sentinel; mirror NativeBinding's
            // start-at-1 convention.
            chassis_correlation: AtomicU64::new(1),
            quit_requested: false,
            terminal_reached: false,
        };
        let app = DesktopWindowApplication::new(window_slot, integration, initial_window);

        Ok(DesktopDriverRunning {
            app,
            event_loop,
            // `boot` stays alive on the running so its scheduler joins
            // workers on drop. Drop ordering on
            // `DesktopDriverRunning::run` exit: app → event_loop → _boot,
            // which means capabilities (held by `app`, including the pumped
            // render + window slots) tear down before the scheduler joins.
            _boot: boot,
        })
    }
}

impl DriverRunning for DesktopDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self {
            mut app,
            event_loop,
            // Held to the end of `run()` so the scheduler joins workers on
            // drop; the `_` prefix keeps the binding alive without a use.
            _boot,
        } = *self;

        event_loop.run_app(&mut app).map_err(|e| RunError::Other(format!("event loop: {e}").into()))?;

        let (started, frame, total) = app.integration().metrics();

        // ADR-0160 §Decision 3 / ADR-0161: run each pumped actor's Closed-path
        // teardown (residual drain, `unwire`, cost-row drop, registry finalize +
        // monitor fan-out) — the `unwire` the bespoke driver drain never had.
        // The render slot's `unwire` logs the triangle count; the window slot's
        // runs the window teardown. The driver boots last, so both land before
        // the chassis tears down each passive in reverse boot order (`_boot`
        // drops at the end of this fn).
        app.integration_mut().shutdown();
        app.shutdown();

        let elapsed = started.map(|started| started.elapsed()).unwrap_or_default();
        // Frame count cast to f64 for FPS report — runs at shutdown,
        // bounded well below 2^53.
        #[allow(clippy::cast_precision_loss)]
        let fps = frame as f64 / elapsed.as_secs_f64().max(0.001);
        tracing::info!(
            target: "aether_substrate::shutdown",
            frames = frame,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            fps = fps,
            triangles = total,
            "frame loop exited",
        );
        Ok(())
    }
}
