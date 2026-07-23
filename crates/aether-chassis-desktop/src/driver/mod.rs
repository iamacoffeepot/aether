//! Desktop chassis driver capability — ADR-0071 phase 3 / ADR-0161 R3.
//!
//! Holds the temporary `DesktopWindowCompatibilityBridge` and the
//! `ApplicationHandler` impl that drives per-frame work. Post-ADR-0161 the
//! driver is a pure pump host: it owns two
//! pumped slots on the winit thread — `aether.window` (ADR-0160) and
//! `aether.render` (this slice) — booting each via
//! [`DriverCtx::boot_pumped_actor`] from a Claim-stage reservation, pushing
//! them mail, and draining. There is no GPU code here: the wgpu surface,
//! accumulators, and pending capture are plain state on the pumped render
//! actor (`aether-render`). `DesktopDriverCapability` composes one driver
//! alongside the passive capabilities (`LogCapability`, `FsCapability`,
//! `HttpCapability`, `AudioCapability`, …); render no longer composes as a
//! passive on desktop.
//!
//! `DesktopDriverRunning::run` blocks on `event_loop.run_app(&mut app)`, runs
//! each pumped slot's `shutdown` teardown on exit, and emits the shutdown
//! telemetry. Returning means the user closed the window or the event loop
//! exited cleanly; the `chassis_builder` then tears down every passive in
//! reverse boot order via `BootedPassives::Drop`.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    DesktopWindowCapability, DesktopWindowParams, WindowCell, WindowHostAction, WindowHostEffect, WindowSizeRequest,
    WindowSpec,
};
use crossbeam_channel::{Receiver, Sender};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId as WinitWindowId};

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

/// Temporary adapter between the window-owned winit domain and the current
/// single-target render/lifecycle path. Multi-target render replaces this
/// bridge in issue #3990.
pub struct DesktopWindowCompatibilityBridge {
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
    window: Option<Arc<Window>>,
    /// Engine identity of the one window the compatibility render target can
    /// present. The window manager may own additional windows; #3990 gives
    /// render a target map for them.
    window_id: Option<EngineWindowId>,
    /// The pumped `aether.render` runtime (ADR-0161 §Decision 1), dispatched
    /// on this winit thread through a [`PumpedSlot`] — the same pump home the
    /// `aether.window` actor uses. It owns the wgpu surface, the accumulators,
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
    pub(crate) started: Option<Instant>,
    pub(crate) frame: u64,
    occluded: bool,
    /// Lowered window boot knobs (mode / size / title / wireframe), parsed
    /// from `AETHER_WINDOW_MODE` / `AETHER_WINDOW_TITLE` / `AETHER_WIREFRAME`
    /// at boot and applied when `resumed` creates the window. Kept so the
    /// window attributes can reference the mode even when `resumed` fires
    /// lazily (and for logging); the size is consulted only when the mode is
    /// `Windowed`. Runtime `set_window_title` mail overrides the title but
    /// doesn't update this field — the current title lives on the `Window`
    /// itself. The wireframe value is threaded into the pumped render actor's
    /// [`RenderParams`], whose lazy wgpu boot owns the tri-state parse.
    window_settings: aether_chassis::WindowSettings,
    /// The `aether.window` desktop runtime, pumped on the winit thread
    /// (ADR-0160 §Decision 3). Booted from the Claim-stage `aether.window`
    /// reservation via [`DriverCtx::boot_pumped_actor`]; drained inside the
    /// winit callbacks — inline on the chassis main thread, since winit /
    /// macOS require window mutations there. Mail arrival pokes
    /// `UserEvent::WindowMail` via the claim's
    /// wake slot (iamacoffeepot/aether#1318), so `about_to_wait` runs and
    /// drains even under `ControlFlow::Wait` (set when the window occludes)
    /// — the case `aether.window.focus` most needs, since the loop is
    /// otherwise parked until a winit event arrives. The shared
    /// `dispatch_envelope` body owns the framework arms (`aether.log.tail` /
    /// `aether.trace.tail` / `aether.cost.tail`), cost fold, and trace hops,
    /// so `aether.window` reports identically to any pooled actor;
    /// [`PumpedSlot::shutdown`] runs the actor's `unwire` on loop exit.
    window_slot: PumpedSlot<DesktopWindowCapability>,
    /// One-shot handle retained strictly for the current render actor: the
    /// first successful compatibility attachment fills it, and render reads
    /// the winit `Window` while #3990 replaces this single-target seam.
    window_cell: WindowCell,
    /// ADR-0080 §6 chassis-root correlation counter (issue
    /// iamacoffeepot/aether#723). Bumped per chassis-source push so
    /// every chassis-owned render/lifecycle emission carries a fresh
    /// `MailId` for the trace observer to root a tree on. Symmetric
    /// with the per-actor counter on `NativeBinding`.
    chassis_correlation: AtomicU64,
    /// True once a graceful-shutdown `Quit` has been pushed to
    /// `aether.lifecycle` (iamacoffeepot/aether#1489), via either
    /// `WindowEvent::CloseRequested` or an observed SIGINT/SIGTERM.
    /// Guards [`DesktopWindowCompatibilityBridge::request_quit`] so the
    /// `Quit` mail is pushed exactly
    /// once, and bypasses the `RedrawRequested` occlusion early-return so
    /// the shutdown frame still drives the lifecycle to its `Shutdown`
    /// terminal even on a minimized/hidden window.
    quit_requested: bool,
    /// SIGINT/SIGTERM shutdown flag, flipped by the signal-watcher
    /// installed in [`DesktopDriverCapability::boot`]
    /// (iamacoffeepot/aether#1489). Polled at the top of
    /// [`ApplicationHandler::about_to_wait`]; on first observation the
    /// driver runs the same `Quit`-push path as `CloseRequested`. A
    /// struct field (mirroring headless's flag) so the watcher and the
    /// winit loop share one source of truth.
    shutdown: Arc<AtomicBool>,
}

impl DesktopWindowCompatibilityBridge {
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

    /// Begin graceful shutdown (iamacoffeepot/aether#1489). Pushes a
    /// chassis-root [`Quit`] mail to `aether.lifecycle` (which sets the
    /// cap's `quit_pending`), marks `quit_requested`, and pokes a redraw
    /// so the `RedrawRequested` advance loop runs. The cap consumes the
    /// quit at its `Present` stage (ADR-0082 §3) — so the in-flight
    /// `Tick → Render → Present` frame finishes composing — then advances
    /// to the `Shutdown` terminal; the advance loop's terminal break
    /// drives `event_loop.exit()` (settle-then-exit, ADR-0082 §11).
    ///
    /// Idempotent on `quit_requested`: the bridges (`CloseRequested`, the
    /// signal flag, `UserEvent::Quit`) can all fire, but `Quit` is pushed
    /// once. The set flag also bypasses the `RedrawRequested` occlusion
    /// early-return so a shutdown requested on a hidden/minimized window
    /// still drives the lifecycle to `Shutdown`.
    fn request_quit(&mut self) {
        if self.quit_requested {
            return;
        }
        self.quit_requested = true;
        self.push_chassis_root(self.lifecycle_mailbox, <Quit as Kind>::ID, encode_empty::<Quit>(), 1);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Mint a chassis-root `LifecycleAdvance` and push it to the
    /// `aether.lifecycle` cap with [`Self::lifecycle_reply_mailbox`] as
    /// its `Component` reply target (issue 1378). Open-codes the
    /// chassis-root push (`push_chassis_root_mail` doesn't carry a reply
    /// target): mint id → record `Sent` for the trace subtree → push with
    /// both the chassis-root lineage and the reply-to. Returns the minted
    /// chain root so the caller can subscribe its settlement.
    fn push_lifecycle_advance(&self) -> MailId {
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
                encode_empty::<aether_kinds::LifecycleAdvance>(),
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

    /// Set the window's occlusion state (ADR-0161 §Decision 4). Forwards the
    /// transition to the pumped render actor as [`Occluded`] mail — its
    /// `on_occluded` handler fail-fasts any pending capture (issue 1317,
    /// relocated into the actor) — and parks / unparks the loop. `Wait` when
    /// occluded is the power-save; `Poll` + a redraw poke on un-occlude
    /// resumes rendering.
    fn set_occluded(&mut self, id: EngineWindowId, occluded: bool, event_loop: &ActiveEventLoop) {
        if self.window_id != Some(id) {
            return;
        }
        if self.occluded == occluded {
            return;
        }
        self.occluded = occluded;
        self.send_render_and_drain(&Occluded { occluded });
        if occluded {
            event_loop.set_control_flow(ControlFlow::Wait);
        } else {
            event_loop.set_control_flow(ControlFlow::Poll);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
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
    fn run_frame_advance(&mut self) -> bool {
        // Clone the registry Arc up front so the per-advance subscription
        // doesn't borrow `self.queue` across the `&mut self.render_slot` the
        // pumped wait needs.
        let registry = self.queue.settlement_registry().cloned();
        loop {
            let advance_root = self.push_lifecycle_advance();
            if let Some(registry) = &registry {
                let pump_tx = self.render_pump_tx.clone();
                registry.subscribe_settlement_with(advance_root, move || {
                    let _ = pump_tx.send(PumpWake::Settled);
                });
                // A frame chain that doesn't settle is a wedged dispatcher —
                // the same fail-fast disposition the pooled drain barrier had
                // (ADR-0063), with the escalating-patience bookkeeping of
                // issue #1305 carried through the pumped wait.
                if let WaitOutcome::Wedged(wedge) = await_settlement_pumped(
                    &self.render_pump_rx,
                    &mut self.render_slot,
                    "desktop.frame_advance",
                    frame_loop::DRAIN_BUDGET,
                    FRAME_SETTLEMENT_CAP,
                    TerminalDisposition::Abort,
                ) {
                    runtime_lifecycle::fatal_abort(&self.outbound, wedge.reason());
                }
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

    /// Park the loop after a drain (ADR-0161 §Decision 4). When visible,
    /// request the next redraw so rendering stays continuous. Then, reading
    /// the pumped actor's pending-capture deadline through the read-only
    /// [`PumpedSlot::read_state`] accessor, park with
    /// `ControlFlow::WaitUntil(deadline)` when a capture is parked — the
    /// single capture-awareness the driver retains, closing the ADR's
    /// parked-window hole so a wedged pre-chain still reaches the deadline
    /// check. A pending redraw (the visible path) takes precedence over the
    /// `WaitUntil`, so continuous rendering is unaffected; the deadline only
    /// bites once the loop would otherwise sit idle.
    fn park_after_drain(&mut self, event_loop: &ActiveEventLoop) {
        if !self.occluded
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        if let Some(Some(deadline)) = self.render_slot.read_state(RenderCapabilityState::capture_deadline) {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        }
    }

    fn initial_window_spec(&self) -> WindowSpec {
        WindowSpec {
            title: self.window_settings.title.clone(),
            mode: self.window_settings.mode.clone(),
            size: self.window_settings.size.map(|(width, height)| WindowSizeRequest { width, height }),
        }
    }

    fn take_window_work(&mut self) -> (Vec<WindowHostAction>, Vec<WindowHostEffect>) {
        self.window_slot.drain_available();
        self.window_slot.host_turn(|state, _ctx| state.take_host_work()).unwrap_or_default()
    }

    fn apply_window_work(
        &mut self,
        event_loop: &ActiveEventLoop,
        actions: Vec<WindowHostAction>,
        effects: Vec<WindowHostEffect>,
    ) {
        let mut dirty = BTreeSet::new();
        self.apply_window_effects(event_loop, effects, &mut dirty);

        for action in actions {
            match &action {
                WindowHostAction::Create { id, .. } => {
                    let id = *id;
                    match action.realize(event_loop) {
                        Ok(Some(window)) => {
                            match self.window_slot.host_turn(|state, _ctx| state.stage_created_window(id, window)) {
                                Some(Ok(created)) => {
                                    self.apply_window_effects(event_loop, vec![created], &mut dirty);
                                }
                                Some(Err(error)) => {
                                    let effects = self
                                        .window_slot
                                        .host_turn(|state, _ctx| state.fail_window_creation(id, error))
                                        .unwrap_or_default();
                                    self.apply_window_effects(event_loop, effects, &mut dirty);
                                }
                                None => {}
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let effects = self
                                .window_slot
                                .host_turn(|state, _ctx| state.fail_window_creation(id, error))
                                .unwrap_or_default();
                            self.apply_window_effects(event_loop, effects, &mut dirty);
                        }
                    }
                }
                WindowHostAction::Close { id } => {
                    self.apply_window_effects(event_loop, vec![WindowHostEffect::Closing { id: *id }], &mut dirty);
                    let effects = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_close(*id, ctx))
                        .unwrap_or_default();
                    self.apply_window_effects(event_loop, effects, &mut dirty);
                }
            }
        }

        self.render_dirty_windows(event_loop, &dirty);
    }

    fn apply_window_effects(
        &mut self,
        event_loop: &ActiveEventLoop,
        effects: Vec<WindowHostEffect>,
        dirty: &mut BTreeSet<EngineWindowId>,
    ) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                WindowHostEffect::Created { id, window } => {
                    let attachment = self.attach_compatibility_window(id, window);
                    let follow_up = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_attachment(id, attachment, ctx))
                        .unwrap_or_default();
                    effects.extend(follow_up);
                }
                WindowHostEffect::Closing { .. } => {
                    // The current render actor has no target-keyed detach
                    // ingress. #3990 replaces this no-op with an explicit
                    // detach before manager removal.
                }
                WindowHostEffect::Dirty { id } => {
                    dirty.insert(id);
                }
                WindowHostEffect::Occluded { id, occluded } => {
                    self.set_occluded(id, occluded, event_loop);
                }
                WindowHostEffect::LastWindowClosed => {
                    self.request_quit();
                    if let Some(id) = self.window_id {
                        dirty.insert(id);
                    } else {
                        // A boot-window creation/attachment failure leaves no
                        // render target that could advance lifecycle to its
                        // terminal. Exit instead of parking before frame one.
                        event_loop.exit();
                    }
                }
            }
        }
    }

    fn attach_compatibility_window(&mut self, id: EngineWindowId, window: Arc<Window>) -> Result<(), String> {
        if self.window.is_some() {
            // Until #3990, additional windows participate in manager identity,
            // controls, and input routing but share no render target.
            return Ok(());
        }
        if let Some(existing) = self.window_cell.get() {
            if !Arc::ptr_eq(existing, &window) {
                return Err("compatibility WindowCell already contains a different window".to_owned());
            }
        } else {
            let _ = self.window_cell.set(Arc::clone(&window));
        }
        self.window = Some(window);
        self.window_id = Some(id);
        self.started.get_or_insert_with(Instant::now);
        Ok(())
    }

    fn render_dirty_windows(&mut self, event_loop: &ActiveEventLoop, dirty: &BTreeSet<EngineWindowId>) {
        if self.window_id.is_none_or(|id| !dirty.contains(&id)) || (self.occluded && !self.quit_requested) {
            return;
        }

        let reached_terminal = self.run_frame_advance();
        self.send_render_and_drain(&Frame { replay_cache_when_idle: false });
        self.frame += 1;
        if reached_terminal {
            event_loop.exit();
            return;
        }
        self.park_after_drain(event_loop);
    }
}

impl ApplicationHandler<UserEvent> for DesktopWindowCompatibilityBridge {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (actions, effects) = self.take_window_work();
        self.apply_window_work(event_loop, actions, effects);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        // `WindowMail` is the generic "a pumped slot took mail — turn the
        // loop" wake (ADR-0161 generalized the ADR-0160 window rule to every
        // pumped slot): both the `aether.window` and `aether.render` slot
        // wakes send it, so `about_to_wait` drains both even under
        // `ControlFlow::Wait` (iamacoffeepot/aether#1318). It pokes a redraw
        // to wake the loop; the drain handlers do the work. `Quit`
        // (iamacoffeepot/aether#1489) is the wake-only signal for the
        // shutdown-flag poll.
        match event {
            UserEvent::WindowMail => {
                let (actions, effects) = self.take_window_work();
                self.apply_window_work(event_loop, actions, effects);
                self.render_slot.drain_available();
            }
            UserEvent::Quit => {
                // iamacoffeepot/aether#1489: the signal-watcher thread
                // flips the shutdown flag and sends this to wake a parked
                // (`ControlFlow::Wait`, occluded) loop. The flag-poll in
                // `about_to_wait` does the actual `Quit`-push; this arm is
                // the wake only, mirroring `WindowMail`.
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WinitWindowId, event: WindowEvent) {
        self.window_slot.drain_available();
        let (actions, effects) = self
            .window_slot
            .host_turn(|state, ctx| {
                state.window_event(window_id, event, ctx);
                state.take_host_work()
            })
            .unwrap_or_default();
        self.apply_window_work(event_loop, actions, effects);
    }

    /// winit fires this between events. Issue 603 Phase 3 makes the
    /// driver itself the cap for `aether.window`, so the per-frame
    /// drain happens here instead of riding through `EventLoopProxy`
    /// from a separate dispatcher thread.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (actions, effects) = self.take_window_work();
        self.apply_window_work(event_loop, actions, effects);
        // ADR-0161: drain the pumped render slot here too, so render mail that
        // arrived while the loop was parked — a `capture_frame` on an occluded
        // window (fail-fast), an `Occluded` forward — is serviced even under
        // `ControlFlow::Wait`, where `RedrawRequested` does not fire. The
        // render slot's mailbox wake pokes `UserEvent::WindowMail` to bring the
        // loop here; the frame path drains it inline, so this is a no-op mid-
        // render.
        self.render_slot.drain_available();
        // iamacoffeepot/aether#1489: poll the SIGINT/SIGTERM flag the
        // signal-watcher flips. On first observation, run the same
        // graceful-shutdown path `CloseRequested` uses. Force
        // `ControlFlow::Poll` so the loop keeps turning until the
        // shutdown frame drives the lifecycle to its `Shutdown` terminal,
        // even if the window was occluded (which had set `Wait`).
        if self.shutdown.load(Ordering::Relaxed) && !self.quit_requested {
            event_loop.set_control_flow(ControlFlow::Poll);
            self.request_quit();
        }
    }
}

/// ADR-0071 driver capability for the desktop chassis. Owns the
/// pieces the winit event-loop body needs at construction time, then
/// `boot()` builds the compatibility bridge plus `DriverRunning`.
/// `boot()` looks up `RenderCapability` via `DriverCtx::expect`
/// (booted earlier in the `.with()` chain) and pulls the accumulator
/// handles out of it.
///
/// The substrate-core handle (`SubstrateBoot`) rides along on the
/// running so the scheduler stays alive for the chassis's lifetime.
pub struct DesktopDriverCapability {
    pub event_loop: EventLoop<UserEvent>,
    pub boot: SubstrateBoot,
    /// Lowered window boot knobs (mode / size / title / wireframe), resolved in
    /// the desktop `Chassis::build` off the source stack and threaded here as a
    /// unit, applied when `resumed` creates the window.
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
    app: DesktopWindowCompatibilityBridge,
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
        ctx.claim_driver_mailbox("aether.window")?;
        ctx.claim_driver_mailbox("aether.render")
    }

    /// ADR-0156 §4: the window boot knobs (`AETHER_WINDOW_MODE` /
    /// `AETHER_WINDOW_TITLE` / `AETHER_WIREFRAME`) belong to the desktop
    /// driver — the driver that owns the winit window — so the chassis config
    /// aggregate carries them only where a window composes (headless, which
    /// drives a std timer, declares no window knob).
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

    // One-shot boot wiring: mailbox claims and bridge construction thread
    // through a single flat sequence;
    // splitting would just pass the same dozen fields through a helper.
    #[allow(clippy::too_many_lines)]
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { event_loop, boot, window, render_config, assets_dir } = self;

        // ADR-0161: the desktop driver boots the pumped `aether.render` actor
        // itself (below). There is no cross-thread render seam — the pumped
        // actor owns the surface, accumulators, and pending capture as plain
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
        let window_cell = WindowCell::default();
        let (window_slot, window_wake_slot) =
            ctx.boot_pumped_actor::<DesktopWindowCapability>((), DesktopWindowParams)?;

        // iamacoffeepot/aether#1318: install an `EventLoopProxy` wake on the
        // claim's wake slot so window-control mail (`focus` / `set_mode` /
        // `set_title`) arriving at an occluded window pokes
        // `UserEvent::WindowMail`, letting winit run `about_to_wait` and drain
        // even under `ControlFlow::Wait`. The proxy is minted here while
        // `event_loop` is still owned by the capability (it moves into
        // `DesktopDriverRunning` after `boot`) — the winit event loop that
        // could not exist at claim time is present at Start, which is exactly
        // why the wake install is Start-stage.
        let window_mail_proxy = event_loop.create_proxy();
        window_wake_slot.set(Arc::new(move || {
            let _ = window_mail_proxy.send_event(UserEvent::WindowMail);
        }));

        // ADR-0161 §Decision 1/3: boot the pumped `aether.render` actor from
        // the driver's Claim-stage `aether.render` reservation. The one-shot
        // `WindowCell` is now strictly a compatibility seam between this
        // bridge and the still-single-target render actor; #3990 replaces it
        // with explicit attach/detach host ingress.
        let (render_slot, render_wake_slot) = ctx.boot_pumped_actor::<RenderCapability>(
            render_config,
            RenderParams {
                observed_kinds: None,
                assets_dir: Some(assets_dir),
                window: Some(window_cell.clone()),
                // ADR-0161 R4: the desktop chassis boots windowed, not offscreen.
                offscreen_size: None,
                wireframe: window.wireframe.clone(),
            },
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

        // iamacoffeepot/aether#1489: install the SIGINT/SIGTERM →
        // graceful-shutdown bridge. The flag is shared with the compatibility bridge
        // (`about_to_wait` polls it); the watcher sends `UserEvent::Quit`
        // via this proxy to wake a parked loop. Minted here while
        // `event_loop` is still owned by the capability.
        let shutdown = Arc::new(AtomicBool::new(false));
        install_shutdown_handler(&shutdown, event_loop.create_proxy());

        // Issue 1378: claim a dedicated inbox for the cap's
        // `LifecycleAdvanceComplete` replies. The per-frame `Tick →
        // Render` cycle stamps this as the `Component` reply target on
        // each `LifecycleAdvance` and drains the receiver synchronously
        // to gate the next advance (see `recv_lifecycle_advance_next`).
        let lifecycle_reply_claim = ctx.claim_mailbox("aether.lifecycle.advance_reply")?;

        let mut app = DesktopWindowCompatibilityBridge {
            queue: Arc::clone(&boot.queue),
            lifecycle_mailbox,
            kind_lifecycle_advance,
            lifecycle_reply_inbox: lifecycle_reply_claim.inbox,
            lifecycle_reply_mailbox: lifecycle_reply_claim.id,
            outbound: Arc::clone(&boot.outbound),
            window: None,
            window_id: None,
            render_slot,
            render_mailbox,
            render_pump_tx,
            render_pump_rx,
            started: None,
            frame: 0,
            occluded: false,
            window_settings: window,
            window_slot,
            window_cell,
            // 0 is the "no correlation" sentinel; mirror NativeBinding's
            // start-at-1 convention.
            chassis_correlation: AtomicU64::new(1),
            quit_requested: false,
            shutdown,
        };
        let initial = app.initial_window_spec();
        let _ = app.window_slot.host_turn(|state, _ctx| {
            let _ = state.queue_initial_window(initial);
        });

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

        // Read the cumulative triangle count off the pumped render actor's
        // plain state through the read-only accessor before `shutdown` consumes
        // the actor (ADR-0161: the driver's shutdown FPS report keeps working
        // off `read_state`; the render actor's `unwire` logs the same count).
        let total = app.render_slot.read_state(RenderCapabilityState::triangles_rendered).unwrap_or(0);

        // ADR-0160 §Decision 3 / ADR-0161: run each pumped actor's Closed-path
        // teardown (residual drain, `unwire`, cost-row drop, registry finalize +
        // monitor fan-out) — the `unwire` the bespoke driver drain never had.
        // The render slot's `unwire` logs the triangle count; the window slot's
        // runs the window teardown. The driver boots last, so both land before
        // the chassis tears down each passive in reverse boot order (`_boot`
        // drops at the end of this fn).
        app.render_slot.shutdown();
        app.window_slot.shutdown();

        let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
        // Frame count cast to f64 for FPS report — runs at shutdown,
        // bounded well below 2^53.
        #[allow(clippy::cast_precision_loss)]
        let fps = app.frame as f64 / elapsed.as_secs_f64().max(0.001);
        tracing::info!(
            target: "aether_substrate::shutdown",
            frames = app.frame,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            fps = fps,
            triangles = total,
            "frame loop exited",
        );
        Ok(())
    }
}
