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
    ActorInitError, ActorTypeTag, Mail, MailboxId, Manual, OutboundReply, PriorState, ReplyHandle,
    SpawnError, Subname, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor,
};
use aether_capabilities::FsCapability;
use aether_capabilities::fs::{FsMailboxExt, ReadResult};
use aether_data::KindId;
use wasmi::Engine;

use crate::envelope::EffectTarget;
use crate::host::config::{HostConfig, LoadScript, LoadScriptResult, ScriptSource, SetScript};
use crate::host::drain::{DrainEvent, DrainSink, EchoGuard, echo_kinds, run_drain};
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
    /// One-shot suppression of the up-lane echoes of the script's own writes.
    echo: EchoGuard,
    /// The current script source (may diverge from `config.script` after a
    /// swap); persisted so a reload records where the script came from.
    script_source: ScriptSource,
    /// The parked reply target of an in-flight `load_script`, discharged when
    /// its `aether.fs.read` settles.
    pending_reply: Option<ReplyHandle>,
    /// The `FsRef` source of an in-flight `load_script`, recorded onto
    /// `script_source` when the load succeeds.
    pending_source: Option<ScriptSource>,
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
            echo: EchoGuard::default(),
            script_source,
            pending_reply: None,
            pending_source: None,
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

        if let ScriptSource::FsRef { namespace, path } = &self.script_source {
            ctx.actor::<FsCapability>().read(namespace, path);
        }

        // Ask the wrapped widget to re-emit its observable kinds up-lane so the
        // script's mirror is primed (the reply arrives as ordinary up-lane
        // traffic through the fallback).
        if let Some(child) = ctx.child(&self.config.child.subname) {
            child.send_bytes(sentinel::REPORT, &[]);
        }
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
        let script_bytes = self
            .slot
            .as_ref()
            .map(|s| s.bytes().to_vec())
            .unwrap_or_default();
        let script_state = self
            .slot
            .as_mut()
            .map_or_else(Vec::new, ScriptSlot::save_state);
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
    /// script, and restore the wrapped-child id for the direction check. The
    /// wrapped child is **not** re-spawned: the composite walk reconstructs it
    /// from its own real config + runtime state (#2694), and the reload
    /// `insert_child` carries no residency guard, so a host-side re-spawn would
    /// double-spawn. The rehydrate body (the private `apply_rehydrate`) takes
    /// no spawn surface, so the defer-to-the-walk invariant is enforced by the
    /// signature, not just discipline.
    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        self.apply_rehydrate(prior.bytes());
    }

    /// Load a script from an `aether.fs` namespace. Parks the requester's
    /// reply target across the async read; `on_read_result` discharges it.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::manual]
    fn on_load_script(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: LoadScript) {
        self.pending_reply = ctx.reply_target();
        self.pending_source = Some(ScriptSource::FsRef {
            namespace: msg.namespace.clone(),
            path: msg.path.clone(),
        });
        ctx.actor::<FsCapability>().read(&msg.namespace, &msg.path);
    }

    /// The fs read reply for a `load_script` (or the boot `FsRef` fetch). Swaps
    /// the script on `Ok`, keeps the prior on `Err`, and discharges the parked
    /// `load_script_result` reply if one is pending.
    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, reply: ReadResult) {
        let result = match reply {
            ReadResult::Ok { bytes, .. } => self.swap_script(&bytes),
            ReadResult::Err { error, .. } => Err(alloc::format!("read failed: {error:?}")),
        };
        match &result {
            Ok(_) => {
                if let Some(source) = self.pending_source.take() {
                    self.script_source = source;
                }
            }
            Err(detail) => {
                self.pending_source = None;
                tracing::warn!(
                    target: "aether_behavior",
                    error = %detail,
                    "load_script failed; keeping the prior running script",
                );
            }
        }
        if let Some(handle) = self.pending_reply.take() {
            ctx.reply_to(handle, &load_result(result));
        }
    }

    /// Swap the script for inline bytes — the synchronous counterpart of
    /// `load_script`. The return value *is* the `load_script_result` reply.
    #[handler::single]
    fn on_set_script(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetScript) -> LoadScriptResult {
        let result = self.swap_script(&msg.bytes);
        if result.is_ok() {
            self.script_source = ScriptSource::Inline(msg.bytes);
        } else if let Err(detail) = &result {
            tracing::warn!(
                target: "aether_behavior",
                error = %detail,
                "set_script failed; keeping the prior running script",
            );
        }
        load_result(result)
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
        if is_up && self.echo.take(kind) {
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
    /// Whether an inbound source is the wrapped child (up-lane); any other
    /// source — the parent, or a sourceless dispatch — is down-lane.
    fn lane_is_up(&self, source: Option<MailboxId>) -> bool {
        matches!((source, self.wrapped_child), (Some(s), Some(w)) if s == w)
    }

    fn offers_kind_to_script(&self, kind: KindId) -> bool {
        self.slot.as_ref().is_some_and(|slot| {
            !slot.is_disabled() && (slot.handles(kind) || self.config.is_mirror_kind(kind))
        })
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
        self.wrapped_child =
            (bundle.wrapped_child_id != 0).then_some(MailboxId(bundle.wrapped_child_id));
        if !bundle.script_bytes.is_empty() {
            match ScriptSlot::instantiate(
                &self.engine,
                &bundle.script_bytes,
                Some(&bundle.script_state),
                self.config.fuel_per_call,
                self.config.disable_after_traps,
            ) {
                Ok(slot) => self.slot = Some(slot),
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
        let Some(slot) = self.slot.as_mut() else {
            return false;
        };
        let output = match slot.filter(offer_kind, bytes) {
            FilterOutcome::Output(output) => output,
            FilterOutcome::Passthrough => return false,
        };
        let echoes = echo_kinds(&output);
        let subname = self.config.child.subname.clone();
        let mut sink = LaneSink {
            ctx,
            wrapped_subname: &subname,
            is_up,
        };
        run_drain(output, forward_kind, &mut sink);
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
    match ScriptSlot::instantiate(
        engine,
        bytes,
        prior_state,
        config.fuel_per_call,
        config.disable_after_traps,
    ) {
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
            DrainEvent::Effect {
                target,
                kind_id,
                bytes,
            } => {
                let relative = match target {
                    EffectTarget::Widget => self.ctx.child(self.wrapped_subname),
                    EffectTarget::Child(path) => self.ctx.child(&path),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::config::ChildSpec;
    use crate::host::test_support::{fixed_output_wasm, forward_output};
    use aether_actor::Lifecycle;
    use aether_actor::wasm::{NO_INBOUND_SOURCE, inline::Registry};
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn config(script: ScriptSource) -> HostConfig {
        HostConfig {
            child: ChildSpec {
                type_tag: 0xAAAA,
                subname: "widget".to_string(),
                config: Vec::new(),
            },
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
            host.run_filter_and_drain(
                ctx.as_single(),
                false,
                Some(mirror_kind),
                mirror_kind,
                b"mirror",
            ),
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
        assert!(
            host.slot.is_some(),
            "the inline boot script should be resident"
        );

        let before = host.slot.as_ref().map(|s| s.bytes().to_vec());
        let result = host.swap_script(b"not wasm");
        assert!(result.is_err());
        let after = host.slot.as_ref().map(|s| s.bytes().to_vec());
        assert_eq!(before, after, "the prior script must survive a failed swap");
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
        assert!(
            host.slot.is_some(),
            "the resident script re-instantiates on reload"
        );
        assert!(matches!(host.script_source, ScriptSource::Inline(_)));
    }

    // Tripwire: an undecodable state blob boots the script fresh (fail-open)
    // rather than misreading it — the host keeps whatever `init` set.
    #[test]
    fn rehydrate_with_garbage_boots_fresh() {
        let mut host = host(ScriptSource::None);
        host.apply_rehydrate(b"garbage blob");
        assert!(host.slot.is_none());
        assert!(host.wrapped_child.is_none());
    }
}
