//! The `BehaviorHost` actor (ADR-0137, issue 2687).
//!
//! One non-generic wasm `#[actor]` that interposes at a tree slot: it spawns
//! its wrapped child (by type tag, #2692) as its own inline child, and offers
//! the down-lane and up-lane mail flowing through the slot to a fuel-metered
//! `wasmi` filter call. Its own control vocabulary
//! (`aether.behavior.{load_script,set_script}` + the `aether.fs.read_result`
//! reply) are `#[handler]`s — consumed, never forwarded; all other mail is
//! lane traffic the `#[fallback]` routes by direction, offers to the script
//! when declared, and drains verdict-then-effects with echo suppression.
//!
//! The host is transparent to the causal chain (it forwards in place through
//! the cluster membrane, inheriting the chain) and the script is invisible to
//! settlement. A trap fails open — the in-flight mail forwards untransformed —
//! and persistence defers the wrapped child's reconstruction to the composite
//! reload walk (#2694), re-instantiating only the host's own script.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{
    ActorInitError, ActorTypeTag, Mail, MailboxId, Manual, OutboundReply, PriorState, ReplyHandle, SpawnError, Subname,
    WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor,
};
use aether_data::KindId;
use aether_fs::{FsCapability, Read, ReadResult};
use wasmi::Engine;

use crate::envelope::EffectTarget;
use crate::host::config::{HostConfig, LoadScript, LoadScriptResult, ScriptSource, SetScript};
use crate::host::drain::{DrainEvent, DrainSink, EchoGuard, echo_effects, run_drain};
use crate::host::persist::{HOST_PERSIST_VERSION, HostPersist};
use crate::host::slot::{FilterOutcome, ScriptSlot, build_engine};
use crate::sentinel;

/// The behavior host: a fixed interposer over one wrapped child.
pub struct BehaviorHost {
    config: HostConfig,
    /// One `wasmi::Engine` per host, built at `init` with fuel metering on.
    engine: Engine,
    /// The running script, or `None` when the host runs wrapper-transparent
    /// (no script yet, or a disabled/failed one replaced by nothing).
    slot: Option<ScriptSlot>,
    /// The wrapped child's alias id, recorded from the spawn `Ok` (or the
    /// persisted bundle on reload). `None` ⇒ the host runs wrapper-less.
    wrapped_child: Option<MailboxId>,
    /// A fresh script slot needs one widget `REPORT` replay as soon as the
    /// wrapped child is resident, so its mirror fills from ordinary lane mail.
    prime_pending: bool,
    /// One-shot suppression of the up-lane echoes of the script's own writes.
    echo: EchoGuard,
    /// The current script source (may diverge from `config.script` after a
    /// swap); persisted so a reload records where the script came from.
    script_source: ScriptSource,
}

/// Whether an in-flight script fetch originated from boot wiring or a runtime
/// `load_script` control message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, aether_data::Schema)]
enum ScriptLoadOrigin {
    Boot,
    Runtime,
}

/// Context stored under an `aether.fs.read` correlation while the behavior host
/// waits for script bytes.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
#[kind(name = "aether.behavior.script_load_context")]
struct ScriptLoadContext {
    reply: Option<ReplyHandle>,
    origin: ScriptLoadOrigin,
    source: ScriptSource,
}

/// The reference behavior host. Load it wrapped over a widget slot through the
/// kit's declarative `WidgetKind::BehaviorHost` attachment (issue 2681 / 2692);
/// the full live scripted-behavior e2e is #2688.
#[actor(instanced)]
impl WasmActor for BehaviorHost {
    type Config = HostConfig;
    const NAMESPACE: &'static str = "aether.behavior.host";

    fn init(config: HostConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let engine = build_engine();
        // An inline / none script loads now (no mail surface needed); an
        // `FsRef` defers to `wire`, where the fs fetch can be sent. A failed
        // inline load boots wrapper-transparent (fail-open).
        let slot = match &config.script {
            ScriptSource::Inline(bytes) => try_instantiate(&engine, bytes, None, &config),
            ScriptSource::FsRef { .. } | ScriptSource::None => None,
        };
        let script_source = config.script.clone();
        Ok(BehaviorHost {
            config,
            engine,
            slot,
            wrapped_child: None,
            prime_pending: false,
            echo: EchoGuard::default(),
            script_source,
        })
    }

    /// Genuine first attach only (reload runs `init` + `on_rehydrate`, not
    /// `wire`): spawn the wrapped child by tag, kick an `FsRef` boot fetch,
    /// prime the widget with a re-emit request, and offer the ATTACH sentinel.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        match ctx.spawn_inline_child_by_tag(
            ActorTypeTag(self.config.child.type_tag),
            Subname::Named(&self.config.child.subname),
            &self.config.child.config,
        ) {
            Ok(id) => self.wrapped_child = Some(id),
            Err(SpawnError::UnknownActorTag(tag)) => {
                tracing::warn!(
                    target: "aether_behavior",
                    type_tag = tag.0,
                    subname = %self.config.child.subname,
                    "wrapped child tag unknown to the module; running wrapper-less (fail-open)",
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_behavior",
                    subname = %self.config.child.subname,
                    ?error,
                    "wrapped child spawn failed; running wrapper-less (fail-open)",
                );
            }
        }

        if let ScriptSource::FsRef { namespace, path } = self.script_source.clone() {
            let read = Read { namespace: namespace.clone(), path: path.clone() };
            let context = ScriptLoadContext {
                reply: None,
                origin: ScriptLoadOrigin::Boot,
                source: ScriptSource::FsRef { namespace, path },
            };
            let _ = ctx.actor::<FsCapability>().send_with_context(&read, &context);
        }

        if self.slot.is_some() {
            self.prime_pending = true;
        }
        self.try_prime(&*ctx);
        self.offer_sentinel(&*ctx, sentinel::ATTACH);
    }

    /// Best-effort teardown: offer the DETACH sentinel as the host leaves.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        self.offer_sentinel(&*ctx, sentinel::DETACH);
    }

    /// Save the host bundle (script source + resident bytes + `state_save`
    /// blob + wrapped-child id) into the host's own parent state. The wrapped
    /// child persists itself through the composite walk (#2694).
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let script_bytes = self.slot.as_ref().map(|s| s.bytes().to_vec()).unwrap_or_default();
        let script_state = self.slot.as_mut().map_or_else(Vec::new, ScriptSlot::save_state);
        let bundle = HostPersist {
            script_source: self.script_source.clone(),
            script_bytes,
            script_state,
            wrapped_child_id: self.wrapped_child.map_or(0, |id| id.0),
        };
        ctx.save_state(u32::from(HOST_PERSIST_VERSION), &bundle.encode());
    }

    /// Restore from the host bundle — re-instantiate the script from its
    /// resident bytes (no fs re-fetch), offer the saved state to the fresh
    /// script, restore the wrapped-child id for the direction check, then offer
    /// `ATTACH` when a script was restored. The wrapped child is **not**
    /// re-spawned: the composite walk reconstructs it from its own real config
    /// + runtime state (#2694), and the reload `insert_child` carries no
    /// residency guard, so a host-side re-spawn would double-spawn. The
    /// rehydrate body (the private `apply_rehydrate`) takes no spawn surface,
    /// so the defer-to-the-walk invariant is enforced by the signature, not
    /// just discipline.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        self.apply_rehydrate_with_attach(prior.bytes(), |host| {
            host.offer_sentinel(&*ctx, sentinel::ATTACH);
        });
    }

    /// Load a script from an `aether.fs` namespace. Carries the requester's
    /// reply target through the async read as a request context.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::manual]
    fn on_load_script(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: LoadScript) {
        let read = Read { namespace: msg.namespace.clone(), path: msg.path.clone() };
        let context = ScriptLoadContext {
            reply: ctx.reply_target(),
            origin: ScriptLoadOrigin::Runtime,
            source: ScriptSource::FsRef { namespace: msg.namespace, path: msg.path },
        };
        let _ = ctx.actor::<FsCapability>().send_with_context(&read, &context);
    }

    /// The fs read reply for a `load_script` (or the boot `FsRef` fetch). Swaps
    /// the script on `Ok`, keeps the prior on `Err`, and recovers the parked
    /// `load_script_result` reply from the request context when one is pending.
    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, reply: ReadResult) {
        let Some(context) = ctx.take_context::<ScriptLoadContext>() else {
            return;
        };
        let (result, reply) = self.apply_read_result(reply, context, |host| {
            host.try_prime(&*ctx.as_single());
            host.offer_sentinel(&*ctx.as_single(), sentinel::ATTACH);
        });
        if let Some(handle) = reply {
            ctx.reply_to(handle, &load_result(result));
        }
    }

    /// Swap the script for inline bytes — the synchronous counterpart of
    /// `load_script`. The return value *is* the `load_script_result` reply.
    #[handler::single]
    fn on_set_script(&mut self, ctx: &mut WasmCtx<'_>, msg: SetScript) -> LoadScriptResult {
        self.apply_set_script(msg, |host| {
            host.try_prime(&*ctx);
            host.offer_sentinel(&*ctx, sentinel::ATTACH);
        })
    }

    /// Lane traffic: everything that is not the host's own control vocabulary.
    /// Resolve the direction against the wrapped-child id, skip the interpreter
    /// for undeclared / suppressed kinds, and otherwise offer to the script and
    /// drain the verdict then effects.
    // The `#[fallback]` dispatch ABI hands `mail` by value; the read-only body
    // does not consume it, but the signature is macro-fixed.
    #[allow(clippy::needless_pass_by_value)]
    #[fallback]
    fn on_lane(&mut self, ctx: &mut WasmCtx<'_>, mail: Mail<'_>) {
        self.try_prime(&*ctx);
        let kind = mail.kind();
        let is_up = self.lane_is_up(ctx.source_mailbox());
        let bytes = mail.bytes();

        // A configured down-lane frame trigger offers FRAME to the script
        // before the trigger itself forwards onward.
        if !is_up && self.config.frame_trigger_kind() == Some(kind) {
            self.offer_sentinel(&*ctx, sentinel::FRAME);
        }

        // An up-lane echo of the script's own write forwards raw — never
        // re-offered to the same script's filter.
        if is_up && self.echo.take(kind, bytes) {
            self.forward_raw(&*ctx, is_up, kind, bytes);
            return;
        }

        if !self.offers_kind_to_script(kind) {
            self.forward_raw(&*ctx, is_up, kind, bytes);
            return;
        }

        if !self.run_filter_and_drain(&*ctx, is_up, Some(kind), kind, bytes) {
            // Fail-open passthrough (trap / malformed output).
            self.forward_raw(&*ctx, is_up, kind, bytes);
        }
    }
}

impl BehaviorHost {
    fn try_prime(&mut self, ctx: &WasmCtx<'_>) {
        let Some(child) = ctx.child(&self.config.child.subname) else {
            return;
        };
        self.prime_if_ready(true, || child.send_bytes(sentinel::REPORT, &[]));
    }

    fn prime_if_ready(&mut self, child_resident: bool, send_report: impl FnOnce()) {
        if !self.prime_pending || self.wrapped_child.is_none() || !child_resident {
            return;
        }
        send_report();
        self.prime_pending = false;
    }

    fn apply_read_result(
        &mut self,
        reply: ReadResult,
        context: ScriptLoadContext,
        mut offer_attach: impl FnMut(&mut Self),
    ) -> (Result<u64, String>, Option<ReplyHandle>) {
        let read = match reply {
            ReadResult::Ok { bytes, .. } => Ok(bytes),
            ReadResult::Err { error, .. } => Err(alloc::format!("read failed: {error:?}")),
        };
        let ScriptLoadContext { reply, origin: _origin, source } = context;
        let result = match read {
            Ok(bytes) => self.swap_script(&bytes),
            Err(error) => Err(error),
        };
        match &result {
            Ok(_) => {
                self.script_source = source;
                offer_attach(self);
            }
            Err(detail) => {
                tracing::warn!(
                    target: "aether_behavior",
                    error = %detail,
                    "load_script failed; keeping the prior running script",
                );
            }
        }
        (result, reply)
    }

    fn apply_set_script(&mut self, msg: SetScript, mut offer_attach: impl FnMut(&mut Self)) -> LoadScriptResult {
        let result = self.swap_script(&msg.bytes);
        if result.is_ok() {
            self.script_source = ScriptSource::Inline(msg.bytes);
            offer_attach(self);
        } else if let Err(detail) = &result {
            tracing::warn!(
                target: "aether_behavior",
                error = %detail,
                "set_script failed; keeping the prior running script",
            );
        }
        load_result(result)
    }

    fn apply_rehydrate_with_attach(&mut self, prior_bytes: &[u8], mut offer_attach: impl FnMut(&mut Self)) {
        self.apply_rehydrate(prior_bytes);
        if self.slot.is_some() {
            offer_attach(self);
        }
    }

    /// Whether an inbound source is the wrapped child (up-lane); any other
    /// source — the parent, or a sourceless dispatch — is down-lane.
    fn lane_is_up(&self, source: Option<MailboxId>) -> bool {
        matches!((source, self.wrapped_child), (Some(s), Some(w)) if s == w)
    }

    fn offers_kind_to_script(&self, kind: KindId) -> bool {
        self.slot
            .as_ref()
            .is_some_and(|slot| !slot.is_disabled() && (slot.handles(kind) || self.config.is_mirror_kind(kind)))
    }

    /// Compile + instantiate `bytes`, carrying the prior script's `state_save`
    /// blob into the new script's `state_load`. Returns the resident byte count
    /// on success; keeps the prior running slot on `Err`.
    fn swap_script(&mut self, bytes: &[u8]) -> Result<u64, String> {
        let prior_state = self.slot.as_mut().map(ScriptSlot::save_state);
        let slot = ScriptSlot::instantiate(
            &self.engine,
            bytes,
            prior_state.as_deref(),
            self.config.fuel_per_call,
            self.config.disable_after_traps,
        )?;
        let resident = slot.bytes().len() as u64;
        self.slot = Some(slot);
        self.prime_pending = true;
        Ok(resident)
    }

    /// The reload body, factored ctx-free so it structurally *cannot* spawn a
    /// child (spawn needs a ctx) — the defer-to-the-walk invariant is enforced
    /// by the signature, not just discipline. Decodes the bundle, restores the
    /// script from resident bytes + the saved state, and restores the
    /// wrapped-child id. An undecodable blob boots fresh with a warning.
    fn apply_rehydrate(&mut self, prior_bytes: &[u8]) {
        let Some(bundle) = HostPersist::decode(prior_bytes) else {
            tracing::warn!(
                target: "aether_behavior",
                "host state blob did not decode; booting the script fresh (fail-open)",
            );
            return;
        };
        self.script_source = bundle.script_source;
        self.wrapped_child = (bundle.wrapped_child_id != 0).then_some(MailboxId(bundle.wrapped_child_id));
        if !bundle.script_bytes.is_empty() {
            match ScriptSlot::instantiate(
                &self.engine,
                &bundle.script_bytes,
                Some(&bundle.script_state),
                self.config.fuel_per_call,
                self.config.disable_after_traps,
            ) {
                Ok(slot) => {
                    self.slot = Some(slot);
                    self.prime_pending = true;
                }
                Err(detail) => tracing::warn!(
                    target: "aether_behavior",
                    error = %detail,
                    "resident script failed to re-instantiate on reload; booting fresh",
                ),
            }
        }
    }

    /// Offer a lifecycle sentinel to the script and drain its effects. A
    /// sentinel carries no in-flight mail, so its verdict is ignored (only
    /// effects drain). A no-op when there is no running script.
    fn offer_sentinel(&mut self, ctx: &WasmCtx<'_>, sentinel: KindId) {
        self.run_filter_and_drain(ctx, false, None, sentinel, &[]);
    }

    #[cfg(test)]
    fn offer_sentinel_to_sink(&mut self, sink: &mut impl DrainSink, sentinel: KindId) {
        self.run_filter_and_drain_to_sink(sink, None, sentinel, &[]);
    }

    /// Offer `(offer_kind, bytes)` to the script and, on a well-formed output,
    /// drain it: apply the verdict (forwarding on the `is_up` lane when
    /// `forward_kind` is `Some`) then the effects in order, arming echo
    /// suppression for the writes it emitted. Returns `true` when the script
    /// produced an output (handled), `false` on passthrough / no script so the
    /// caller forwards raw.
    fn run_filter_and_drain(
        &mut self,
        ctx: &WasmCtx<'_>,
        is_up: bool,
        forward_kind: Option<KindId>,
        offer_kind: KindId,
        bytes: &[u8],
    ) -> bool {
        let subname = self.config.child.subname.clone();
        let mut sink = LaneSink { ctx, wrapped_subname: &subname, is_up };
        self.run_filter_and_drain_to_sink(&mut sink, forward_kind, offer_kind, bytes)
    }

    fn run_filter_and_drain_to_sink(
        &mut self,
        sink: &mut impl DrainSink,
        forward_kind: Option<KindId>,
        offer_kind: KindId,
        bytes: &[u8],
    ) -> bool {
        let Some(slot) = self.slot.as_mut() else {
            return false;
        };
        let output = match slot.filter(offer_kind, bytes) {
            FilterOutcome::Output(output) => output,
            FilterOutcome::Passthrough => return false,
        };
        let echoes = echo_effects(&output);
        run_drain(output, forward_kind, sink);
        self.echo.arm(echoes);
        true
    }

    /// Forward the in-flight mail raw (no interpreter call) along its lane —
    /// up to the parent, or down to the wrapped child.
    fn forward_raw(&self, ctx: &WasmCtx<'_>, is_up: bool, kind: KindId, bytes: &[u8]) {
        let relative = if is_up {
            ctx.parent()
        } else {
            ctx.child(&self.config.child.subname)
        };
        if let Some(target) = relative {
            target.send_bytes(kind, bytes);
        } else {
            tracing::warn!(
                target: "aether_behavior",
                kind = kind.0,
                up = is_up,
                "lane forward dropped: no resident relative on that lane",
            );
        }
    }
}

/// Project a swap result into the wire `load_script_result` reply.
fn load_result(result: Result<u64, String>) -> LoadScriptResult {
    match result {
        Ok(resident_bytes) => LoadScriptResult::Ok { resident_bytes },
        Err(error) => LoadScriptResult::Err { error },
    }
}

/// Instantiate `bytes` best-effort — `None` (with a warn) on failure, so a
/// failed inline boot load runs wrapper-transparent rather than aborting.
fn try_instantiate(
    engine: &Engine,
    bytes: &[u8],
    prior_state: Option<&[u8]>,
    config: &HostConfig,
) -> Option<ScriptSlot> {
    match ScriptSlot::instantiate(engine, bytes, prior_state, config.fuel_per_call, config.disable_after_traps) {
        Ok(slot) => Some(slot),
        Err(detail) => {
            tracing::warn!(
                target: "aether_behavior",
                error = %detail,
                "inline boot script failed to instantiate; running wrapper-transparent",
            );
            None
        }
    }
}

/// The ctx-backed drain sink: routes each drain event to a relative cluster
/// handle. The in-flight forward follows the mail's lane; an effect follows its
/// `EffectTarget` (Widget/Child down, Panel up).
struct LaneSink<'c, 'a> {
    ctx: &'c WasmCtx<'a>,
    wrapped_subname: &'c str,
    is_up: bool,
}

impl DrainSink for LaneSink<'_, '_> {
    fn record(&mut self, event: DrainEvent) {
        let (relative, kind_id, bytes) = match event {
            DrainEvent::Forward { kind_id, bytes } => {
                let relative = if self.is_up {
                    self.ctx.parent()
                } else {
                    self.ctx.child(self.wrapped_subname)
                };
                (relative, kind_id, bytes)
            }
            DrainEvent::Effect { target, kind_id, bytes } => {
                let relative = match target {
                    EffectTarget::Widget => self.ctx.child(self.wrapped_subname),
                    EffectTarget::Child(path) => resolve_child_path(self.ctx, &path),
                    EffectTarget::Panel => self.ctx.parent(),
                };
                (relative, kind_id, bytes)
            }
        };
        if let Some(target) = relative {
            target.send_bytes(KindId(kind_id), &bytes);
        } else {
            tracing::warn!(
                target: "aether_behavior",
                kind = kind_id,
                "drain send dropped: no resident relative for that target",
            );
        }
    }
}

fn resolve_child_path<'a>(ctx: &WasmCtx<'a>, path: &str) -> Option<aether_actor::RelativeMailbox<'a>> {
    let mut segments = path.split('/');
    let first = segments.next().filter(|segment| !segment.is_empty())?;
    segments.try_fold(ctx.child(first)?, |relative, segment| {
        if segment.is_empty() {
            None
        } else {
            relative.child(segment)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Effect, FilterOutput, Verdict};
    use crate::host::config::ChildSpec;
    use crate::host::test_support::{fixed_output_wasm, forward_output};
    use aether_actor::Lifecycle;
    use aether_actor::wasm::{NO_INBOUND_SOURCE, inline::Registry};
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    #[derive(Default)]
    struct RecordingSink {
        events: Vec<DrainEvent>,
    }

    impl DrainSink for RecordingSink {
        fn record(&mut self, event: DrainEvent) {
            self.events.push(event);
        }
    }

    fn config(script: ScriptSource) -> HostConfig {
        HostConfig {
            child: ChildSpec { type_tag: 0xAAAA, subname: "widget".to_string(), config: Vec::new() },
            script,
            fuel_per_call: HostConfig::DEFAULT_FUEL_PER_CALL,
            disable_after_traps: HostConfig::DEFAULT_DISABLE_AFTER_TRAPS,
            frame_trigger: 0,
            mirror_kinds: Vec::new(),
        }
    }

    fn host(script: ScriptSource) -> BehaviorHost {
        let mut init_ctx = WasmInitCtx::__new(0x10);
        BehaviorHost::init(config(script), &mut init_ctx).expect("test setup: host inits")
    }

    fn load_context(origin: ScriptLoadOrigin, namespace: &str, path: &str) -> ScriptLoadContext {
        ScriptLoadContext { reply: None, origin, source: fs_ref(namespace, path) }
    }

    fn reply_handle(raw: u32) -> ReplyHandle {
        aether_data::wire::from_bytes(&raw.to_le_bytes())
            .expect("reply handle should decode from its scalar wire shape")
    }

    fn attach_script() -> Vec<u8> {
        fixed_output_wasm(
            sentinel::ATTACH,
            &FilterOutput {
                verdict: Verdict::Consume,
                effects: vec![Effect { target: EffectTarget::Widget, kind_id: 0xA77A, bytes: b"attached".to_vec() }],
            },
        )
    }

    fn attached_script_read_result() -> ReadResult {
        ReadResult::Ok { namespace: "assets".to_string(), path: "behavior.wasm".to_string(), bytes: attach_script() }
    }

    fn assert_attach_offered(sink: &RecordingSink) {
        assert_eq!(
            sink.events,
            vec![DrainEvent::Effect { target: EffectTarget::Widget, kind_id: 0xA77A, bytes: b"attached".to_vec() }],
        );
    }

    fn drain_prime(host: &mut BehaviorHost, reports: &mut u32) {
        host.prime_if_ready(true, || *reports += 1);
    }

    fn fs_ref(namespace: &str, path: &str) -> ScriptSource {
        ScriptSource::FsRef { namespace: namespace.to_string(), path: path.to_string() }
    }

    // Tripwire: the fallback's lane direction — an inbound source equal to the
    // wrapped child is up-lane; the parent, or a sourceless dispatch, is
    // down-lane. A wrong direction would forward mail the opposite way.
    #[test]
    fn lane_direction_resolves_against_wrapped_child() {
        let mut host = host(ScriptSource::None);
        host.wrapped_child = Some(MailboxId(0xC0FFEE));
        assert!(host.lane_is_up(Some(MailboxId(0xC0FFEE))));
        assert!(!host.lane_is_up(Some(MailboxId(0xBEEF))));
        assert!(!host.lane_is_up(None));
        // With no wrapped child every source reads down-lane.
        host.wrapped_child = None;
        assert!(!host.lane_is_up(Some(MailboxId(0xC0FFEE))));
    }

    // Tripwire: low-rate mirror kinds are still offered to SDK dispatch even
    // when the script manifest does not declare a handler for that kind. Once
    // admitted, the fixture returns a passthrough output for the same bytes.
    #[test]
    fn mirror_kind_bypasses_manifest_skip() {
        let declared = KindId(0x1234);
        let mirror_kind = KindId(0x5678);
        let script = fixed_output_wasm(declared, &forward_output(b"mirror"));
        let mut host = host(ScriptSource::Inline(script));
        host.config.mirror_kinds = alloc::vec![mirror_kind.0];

        let slot = host.slot.as_ref().expect("test setup: script resident");
        assert!(slot.handles(declared));
        assert!(!slot.handles(mirror_kind));
        assert!(host.offers_kind_to_script(mirror_kind));

        let registry = Registry::new();
        let mut ctx = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
        assert!(
            host.run_filter_and_drain(ctx.as_single(), false, Some(mirror_kind), mirror_kind, b"mirror",),
            "the undeclared mirror kind should reach guest dispatch"
        );
    }

    // Tripwire: a failed swap (bad bytes) keeps the prior running script and
    // reports `Err` — the control path must never silence the widget by
    // dropping a working script on a bad load.
    #[test]
    fn failed_swap_keeps_prior_script_and_reports_error() {
        let kind = KindId(0x1234);
        let good = fixed_output_wasm(kind, &forward_output(b"ok"));
        let mut host = host(ScriptSource::Inline(good));
        assert!(host.slot.is_some(), "the inline boot script should be resident");

        let before = host.slot.as_ref().map(|s| s.bytes().to_vec());
        let result = host.swap_script(b"not wasm");
        assert!(result.is_err());
        let after = host.slot.as_ref().map(|s| s.bytes().to_vec());
        assert_eq!(before, after, "the prior script must survive a failed swap");
    }

    // Tripwire: a script installed by the deferred fs/load_script path primes
    // its mirror exactly once before ATTACH.
    #[test]
    fn read_result_success_primes_then_offers_attach_after_install() {
        let mut host = host(ScriptSource::None);
        host.wrapped_child = Some(MailboxId(0xC0FFEE));
        let mut sink = RecordingSink::default();
        let mut reports = 0;

        let (result, reply) = host.apply_read_result(
            ReadResult::Ok {
                namespace: "assets".to_string(),
                path: "behavior.wasm".to_string(),
                bytes: attach_script(),
            },
            load_context(ScriptLoadOrigin::Boot, "assets", "behavior.wasm"),
            |host| {
                drain_prime(host, &mut reports);
                host.offer_sentinel_to_sink(&mut sink, sentinel::ATTACH);
            },
        );

        assert!(matches!(result, Ok(_)));
        assert!(reply.is_none());
        assert!(host.slot.is_some());
        assert!(matches!(host.script_source, ScriptSource::FsRef { .. }));
        assert_eq!(reports, 1);
        assert!(!host.prime_pending);
        assert_attach_offered(&sink);
    }

    // Tripwire: a runtime fs reply installs the script only through its explicit
    // request context, not by matching echoed namespace/path fields.
    #[test]
    fn read_result_uses_request_context_for_script_source() {
        let mut host = host(ScriptSource::None);
        let mut sink = RecordingSink::default();

        let (result, reply) = host.apply_read_result(
            ReadResult::Ok { namespace: "assets".to_string(), path: "echoed.wasm".to_string(), bytes: attach_script() },
            load_context(ScriptLoadOrigin::Runtime, "scripts", "actual.wasm"),
            |host| host.offer_sentinel_to_sink(&mut sink, sentinel::ATTACH),
        );

        assert!(matches!(result, Ok(_)));
        assert!(reply.is_none());
        assert!(host.slot.is_some());
        assert_eq!(host.script_source, fs_ref("scripts", "actual.wasm"));
        assert_attach_offered(&sink);
    }

    // Tripwire: a runtime inline swap primes its mirror exactly once before
    // ATTACH after the new slot becomes resident.
    #[test]
    fn set_script_success_primes_then_offers_attach_after_install() {
        let mut host = host(ScriptSource::None);
        host.wrapped_child = Some(MailboxId(0xC0FFEE));
        let mut sink = RecordingSink::default();
        let mut reports = 0;

        let result = host.apply_set_script(SetScript { bytes: attach_script() }, |host| {
            drain_prime(host, &mut reports);
            host.offer_sentinel_to_sink(&mut sink, sentinel::ATTACH);
        });

        assert!(matches!(result, LoadScriptResult::Ok { .. }));
        assert!(host.slot.is_some());
        assert!(matches!(host.script_source, ScriptSource::Inline(_)));
        assert_eq!(reports, 1);
        assert!(!host.prime_pending);
        assert_attach_offered(&sink);
    }

    // Tripwire: `on_rehydrate`'s body restores the wrapped-child id + the
    // script from resident bytes through a ctx-free path — so it structurally
    // cannot re-spawn the wrapped child (spawn needs a ctx), the defer-to-the-
    // walk invariant (#2694). A re-spawn would double-spawn against the
    // unguarded reload insert.
    #[test]
    fn rehydrate_restores_without_spawning() {
        let kind = KindId(0x5678);
        let script = fixed_output_wasm(kind, &forward_output(b"resident"));
        let bundle = HostPersist {
            script_source: ScriptSource::Inline(script.clone()),
            script_bytes: script,
            script_state: Vec::new(),
            wrapped_child_id: 0x1234_5678,
        };

        // A host that booted with no script and no wrapped child.
        let mut host = host(ScriptSource::None);
        assert!(host.slot.is_none());
        assert!(host.wrapped_child.is_none());

        host.apply_rehydrate(&bundle.encode());

        assert_eq!(host.wrapped_child, Some(MailboxId(0x1234_5678)));
        assert!(host.slot.is_some(), "the resident script re-instantiates on reload");
        assert!(matches!(host.script_source, ScriptSource::Inline(_)));
        assert!(host.prime_pending, "rehydrate arms mirror priming for the first resident lane frame");
    }

    // Tripwire: a rehydrate that restores a resident script offers ATTACH
    // immediately but defers mirror priming until the wrapped child is resident.
    #[test]
    fn rehydrate_restored_script_offers_attach_and_defers_prime_until_child_resident() {
        let script = attach_script();
        let bundle = HostPersist {
            script_source: ScriptSource::Inline(script.clone()),
            script_bytes: script,
            script_state: Vec::new(),
            wrapped_child_id: 0x1234_5678,
        };
        let mut host = host(ScriptSource::None);
        let mut sink = RecordingSink::default();
        let mut reports = 0;

        host.apply_rehydrate_with_attach(&bundle.encode(), |host| {
            host.offer_sentinel_to_sink(&mut sink, sentinel::ATTACH);
        });

        assert!(host.slot.is_some());
        assert!(host.prime_pending);
        assert_attach_offered(&sink);

        host.prime_if_ready(false, || reports += 1);
        assert_eq!(reports, 0, "rehydrate waits until the wrapped child is resident");
        assert!(host.prime_pending);

        host.prime_if_ready(true, || reports += 1);
        assert_eq!(reports, 1, "first resident opportunity sends the replay");
        assert!(!host.prime_pending);

        host.prime_if_ready(true, || reports += 1);
        assert_eq!(reports, 1, "priming is exactly-once per restored slot");
    }

    // Tripwire: an undecodable state blob boots the script fresh (fail-open)
    // rather than misreading it — the host keeps whatever `init` set.
    #[test]
    fn rehydrate_with_garbage_boots_fresh() {
        let kind = KindId(0x6789);
        let resident = fixed_output_wasm(kind, &forward_output(b"resident"));
        let mut host = host(ScriptSource::Inline(resident.clone()));
        host.wrapped_child = Some(MailboxId(0xBEEF));

        let before = host.slot.as_ref().map(|slot| slot.bytes().to_vec());
        let before_source = host.script_source.clone();
        host.apply_rehydrate(b"garbage blob");
        let after = host.slot.as_ref().map(|slot| slot.bytes().to_vec());

        assert_eq!(after, before, "garbage rehydrate must keep the resident slot");
        assert_eq!(host.wrapped_child, Some(MailboxId(0xBEEF)), "garbage rehydrate must not clobber the wrapped child",);
        assert_eq!(host.script_source, before_source, "garbage rehydrate must keep the init-time script source",);
    }

    // Tripwire: scriptless or undecodable rehydrate offers no ATTACH because
    // no script slot became resident.
    #[test]
    fn scriptless_rehydrate_offers_no_attach() {
        let mut host = host(ScriptSource::None);
        let mut sink = RecordingSink::default();

        host.apply_rehydrate_with_attach(b"garbage blob", |host| {
            host.offer_sentinel_to_sink(&mut sink, sentinel::ATTACH);
        });

        assert!(host.slot.is_none());
        assert!(!host.prime_pending);
        assert!(sink.events.is_empty());
    }

    // Tripwire: a boot `FsRef` fetch and runtime `load_script` can target the
    // same file while in flight; each reply must resolve by its request context,
    // not by queue order or echoed fields.
    #[test]
    fn boot_and_runtime_same_path_loads_resolve_to_their_own_contexts() {
        let mut host = host(fs_ref("assets", "behavior.wasm"));
        let mut runtime_context = load_context(ScriptLoadOrigin::Runtime, "assets", "behavior.wasm");
        runtime_context.reply = Some(reply_handle(77));
        let boot_context = load_context(ScriptLoadOrigin::Boot, "assets", "behavior.wasm");

        let (runtime_result, runtime_reply) =
            host.apply_read_result(attached_script_read_result(), runtime_context, |_| {});
        assert!(matches!(runtime_result, Ok(_)));
        assert_eq!(runtime_reply.map(ReplyHandle::raw), Some(77));

        let (boot_result, boot_reply) = host.apply_read_result(attached_script_read_result(), boot_context, |_| {});
        assert!(matches!(boot_result, Ok(_)));
        assert!(boot_reply.is_none());
    }
}
