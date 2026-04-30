// Substrate-side control plane for ADR-0010. Reserved mailbox name:
// `aether.control`. Agents drive runtime component loading / dropping
// / replacement by mailing here; the substrate handles each reserved
// kind inline on the sink-handler thread and replies with a
// matching `aether.control.*_result` addressed at the originating
// session.
//
// Surface area: `load_component`, `drop_component`, `replace_component`.
// Each has its own result kind so an agent can disambiguate replies
// without threading a correlation token through the payload.
//
// ADR-0019 PR 5: the on-wire payload types live in
// `aether-kinds` as schema-described kinds (LoadComponent,
// LoadResult, etc.) — no more separate `*Payload` structs in this
// crate. The substrate decodes incoming mail as the kind type
// directly via postcard, reads a component's kind vocabulary from
// its `aether.kinds` wasm custom section (ADR-0028), and replies
// with the matching result kind.
//
// Error discipline: agent-visible failures (bad postcard, kind
// conflict, name conflict, invalid WASM, wasmtime instantiation
// error, unknown/wrong-type mailbox) surface as an `Err` variant on
// the matching result. Panics are reserved for invariant violations
// that the agent cannot have caused — e.g. a poisoned lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_hub_protocol::{EngineToHub, KindDescriptor};
use aether_kinds::{
    ComponentCapabilities, DropComponent, DropResult, InputStream, Key, KeyRelease, LoadComponent,
    LoadResult, MailEnvelope, MouseButton, MouseMove, ReplaceComponent, ReplaceResult,
    SubscribeInput, SubscribeInputResult, Tick, UnsubscribeInput, WindowSize,
};
use aether_mail::{Kind, KindId};
use wasmtime::{Engine, Linker, Module};

use crate::component::Component;
use crate::ctx::SubstrateCtx;
use crate::hub_client::HubOutbound;
use crate::input::{self, InputSubscribers};
use crate::kind_manifest;
use crate::mail::{Mail, MailboxId};
use crate::mailer::Mailer;
use crate::registry::{Registry, SinkHandler};
use crate::scheduler::{ComponentEntry, ComponentTable, close_and_join};

/// Well-known mailbox name for the ADR-0010 control plane. Mail to
/// this name is routed to the control-plane sink handler rather than
/// a component. Kept as a constant so substrate init, tests, and any
/// future tooling share one spelling.
pub const AETHER_CONTROL: &str = "aether.control";

/// ADR-0038 retains `ReplaceComponent::drain_timeout_ms` for wire
/// compatibility but the field is no longer load-bearing: replace is a
/// channel splice, so the "drain" phase is implicit in joining the old
/// dispatcher. Kept as a default for callers that pass `None`.
pub const DEFAULT_DRAIN_TIMEOUT_MS: u32 = 5_000;

/// Postcard-decode a control-plane payload with the one error-message
/// shape every handler uses. Handlers wrap the `String` in their own
/// `*Result::Err` variant — the shape is uniform, the enum differs.
/// `pub` so chassis-side control handlers can reuse the same shape.
pub fn decode_payload<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("postcard decode failed: {e}"))
}

/// Register every descriptor from a component's embedded manifest.
/// Under ADR-0030 Phase 2's hashed ids, `register_kind_with_descriptor`
/// is idempotent on `(name, schema)` match and only fails on a genuine
/// hash collision — which at 64 bits is vanishingly rare. A fresh
/// (name, schema) lands in its own slot; a duplicate with identical
/// schema is a no-op. Two registrations with the same name but
/// different schemas get two distinct ids — producer and consumer
/// will naturally disagree on `K::ID`, surfacing as "kind not found"
/// on the first mail rather than silent data corruption.
fn register_or_match_all(
    registry: &Registry,
    descriptors: &[KindDescriptor],
) -> Result<(), String> {
    for kind in descriptors {
        registry
            .register_kind_with_descriptor(kind.clone())
            .map_err(|e| format!("register `{}`: {e}", kind.name))?;
    }
    Ok(())
}

/// The substrate's six fixed input streams paired with the typed
/// `Kind::ID` constants that drive them. `K::ID` is a compile-time
/// const, so the table folds at const-eval; a rename or schema
/// change on either side trips a compile error here instead of a
/// silent name-string miss.
///
/// Temporary by design — this bridge between the closed `InputStream`
/// enum and `KindId` exists only because `InputStream` is a separate
/// identifier. Issue #405 retires the enum and keys subscribers by
/// `KindId` directly, at which point this table goes away entirely.
const INPUT_STREAM_KINDS: &[(u64, InputStream)] = &[
    (Tick::ID, InputStream::Tick),
    (Key::ID, InputStream::Key),
    (KeyRelease::ID, InputStream::KeyRelease),
    (MouseMove::ID, InputStream::MouseMove),
    (MouseButton::ID, InputStream::MouseButton),
    (WindowSize::ID, InputStream::WindowSize),
];

/// Map an input-kind id to its `InputStream` variant. Used by the
/// load / replace paths to derive auto-subscriptions from the
/// component's `aether.kinds.inputs` manifest, so a component that
/// declares an input handler is wired to the matching stream without
/// the guest SDK needing to round-trip through `subscribe_input` at
/// init time (which races mailbox registration — issue #403).
///
/// User-space input kinds (anything outside the substrate's six
/// fixed streams) return `None`; those continue to ride the explicit
/// `ctx.subscribe_input::<K>()` runtime API.
fn input_stream_for_kind_id(id: KindId) -> Option<InputStream> {
    INPUT_STREAM_KINDS
        .iter()
        .find_map(|(k, s)| (id.0 == *k).then_some(*s))
}

/// Wire the freshly-registered mailbox into every input-stream
/// subscriber set its handler manifest declares. Called by `handle_
/// load` (after `try_register_component`) and `handle_replace` (after
/// the dispatcher swap commits) so the auto-subscribe is observable
/// the moment the component is reachable, with no `subscribe_input`
/// reply to wait on.
fn auto_subscribe_inputs(
    input_subscribers: &InputSubscribers,
    mailbox: MailboxId,
    capabilities: &ComponentCapabilities,
) {
    let mut subs = input_subscribers.write().unwrap();
    for handler in &capabilities.handlers {
        if let Some(stream) = input_stream_for_kind_id(handler.id) {
            subs.entry(stream).or_default().insert(mailbox);
        }
    }
}

/// Shared validation for `subscribe_input` / `unsubscribe_input`: the
/// mailbox id must name a live component. Sinks are rejected because
/// they handle mail inline and have no business receiving fan-out
/// input events; dropped mailboxes are rejected so callers don't
/// unsubscribe with a stale id and think the op succeeded.
fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(crate::registry::MailboxEntry::Component) => Ok(()),
        Some(crate::registry::MailboxEntry::Sink(_)) => {
            Err(format!("mailbox {:?} is a sink, not a component", id))
        }
        Some(crate::registry::MailboxEntry::Dropped) => {
            Err(format!("mailbox {:?} already dropped", id))
        }
        None => Err(format!("unknown mailbox id {:?}", id)),
    }
}

/// Resolve every envelope in `bundle` against the registry, returning
/// fully-typed `Mail`s. On any resolve failure, return a formatted
/// error string tagged with `label` (e.g. `"capture bundle"`); the
/// caller surfaces it as a `CaptureFrameResult::Err`. `pub` so chassis-
/// side handlers can reuse the same mail-envelope validation.
pub fn resolve_bundle(
    registry: &Registry,
    bundle: &[MailEnvelope],
    label: &str,
) -> Result<Vec<Mail>, String> {
    let mut out = Vec::with_capacity(bundle.len());
    for env in bundle {
        let mailbox = registry.lookup(&env.recipient_name).ok_or_else(|| {
            format!(
                "unknown recipient mailbox {:?} in {label}",
                env.recipient_name
            )
        })?;
        let kind_id = registry
            .kind_id(&env.kind_name)
            .ok_or_else(|| format!("unknown kind {:?} in {label}", env.kind_name))?;
        out.push(Mail::new(mailbox, kind_id, env.payload.clone(), env.count));
    }
    Ok(out)
}

/// State the control-plane sink handler captures. Grouping it in a
/// struct keeps the closure body short and makes the dependencies
/// explicit — useful since the handler needs a broad slice of
/// substrate state (wasmtime, registry, scheduler table, outbound).
///
/// `Clone` is cheap — every field is an `Arc` — and exists for tests
/// that want to drive `dispatch` more than once (each call consumes
/// the handler via `into_sink_handler`). Production has exactly one
/// ControlPlane and never clones it.
#[derive(Clone)]
pub struct ControlPlane {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<SubstrateCtx>>,
    pub registry: Arc<Registry>,
    pub queue: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    pub components: ComponentTable,
    /// ADR-0021 per-stream subscriber sets, shared with the platform
    /// thread. The control plane mutates this table on subscribe /
    /// unsubscribe / drop; the platform thread reads it to fan out
    /// each published event.
    pub input_subscribers: InputSubscribers,
    /// Monotonic counter for default component names. Only consulted
    /// when the load payload omits `name`.
    pub default_name_counter: Arc<AtomicU64>,
    /// ADR-0035 chassis fallback. Core's dispatch handles only the
    /// core-concern kinds (load/drop/replace/subscribe/unsubscribe);
    /// anything else falls through to this handler so the chassis
    /// can register its own control-plane surface (capture_frame,
    /// set_window_mode, platform_info on desktop; whatever each
    /// future chassis wants). `None` routes unknown kinds to the
    /// drop-warn log — fine for tests and the hub chassis that
    /// inherits nothing peripheral.
    pub chassis_handler: Option<ChassisControlHandler>,
}

/// Closure contract for a chassis-registered control-plane handler.
/// Called with `(kind_name, sender, bytes)` for every mail arriving
/// at `aether.control` that core's ControlPlane doesn't recognise.
/// The chassis is responsible for decoding, replying (via the
/// outbound it constructed with), and any mail orchestration.
pub type ChassisControlHandler = Arc<dyn Fn(u64, &str, crate::mail::ReplyTo, &[u8]) + Send + Sync>;

impl ControlPlane {
    /// Build the sink handler that should be registered against the
    /// `AETHER_CONTROL` mailbox. The returned `SinkHandler` is
    /// `Send + Sync`; it captures `self` by value (through `Arc`s) so
    /// the caller can discard the `ControlPlane` after registration.
    pub fn into_sink_handler(self) -> SinkHandler {
        Arc::new(
            move |kind_id: u64,
                  kind_name: &str,
                  _origin: Option<&str>,
                  sender: crate::mail::ReplyTo,
                  bytes: &[u8],
                  _count: u32| {
                self.dispatch(kind_id, kind_name, sender, bytes);
            },
        )
    }

    fn dispatch(&self, kind_id: u64, kind_name: &str, sender: crate::mail::ReplyTo, bytes: &[u8]) {
        if kind_id == LoadComponent::ID {
            let result = self.handle_load(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_id == DropComponent::ID {
            let result = self.handle_drop(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_id == ReplaceComponent::ID {
            let result = self.handle_replace(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_id == SubscribeInput::ID {
            let result = self.handle_subscribe(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_id == UnsubscribeInput::ID {
            let result = self.handle_unsubscribe(bytes);
            self.outbound.send_reply(sender, &result);
        } else if let Some(handler) = &self.chassis_handler {
            handler(kind_id, kind_name, sender, bytes);
        } else {
            tracing::warn!(
                target: "aether_substrate::control",
                kind = %kind_name,
                "{AETHER_CONTROL} received unrecognised kind (no chassis handler registered) — dropping",
            );
        }
    }

    fn handle_load(&self, bytes: &[u8]) -> LoadResult {
        let payload: LoadComponent = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return LoadResult::Err { error },
        };

        // ADR-0028: the component's kind vocabulary rides in its
        // wasm `aether.kinds` custom section. Reading before
        // `Module::new` lets a bad manifest fail before we spend
        // cycles compiling, and keeps the "no partial registry
        // state on failure" property — the registry is untouched
        // until every descriptor passes conflict detection.
        let descriptors: Vec<KindDescriptor> = match kind_manifest::read_from_bytes(&payload.wasm) {
            Ok(d) => d,
            Err(error) => return LoadResult::Err { error },
        };
        if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
            return LoadResult::Err { error };
        }

        // ADR-0033: read the receive-side capability surface from the
        // sibling `aether.kinds.inputs` section. Absence is not an
        // error — components predating the `#[handlers]` macro ship
        // an empty `ComponentCapabilities` and the hub treats them as
        // opaque (no structured receive vocabulary to show MCP).
        let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
            Ok(c) => c,
            Err(error) => return LoadResult::Err { error },
        };

        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return LoadResult::Err {
                    error: format!("invalid wasm module: {e}"),
                };
            }
        };

        let name = payload.name.unwrap_or_else(|| {
            let n = self.default_name_counter.fetch_add(1, Ordering::Relaxed);
            format!("component_{n}")
        });

        // ADR-0029: mailbox ids are name-derived (FNV-1a 64), so we
        // can compute the id without touching the registry. That lets
        // `Component::instantiate` (which calls `init(mailbox_id)`)
        // run *before* publishing the mailbox — on instantiate
        // failure the registry is untouched and the name remains
        // available for a retry. Auto-subscriptions are not driven
        // from `init` for that reason (issue #403): a `subscribe_
        // input` mail emitted before `try_register_component` would
        // race against `validate_subscriber_mailbox` and be rejected
        // as an unknown id. Instead, the substrate derives the
        // subscription set from the component's `aether.kinds.inputs`
        // manifest after the mailbox is published — see
        // `auto_subscribe_inputs` below.
        let mailbox = MailboxId::from_name(&name);

        let ctx = SubstrateCtx::new(
            mailbox,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            Arc::clone(&self.outbound),
            Arc::clone(&self.input_subscribers),
        );
        let mut component = match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
            Ok(c) => c,
            Err(e) => {
                return LoadResult::Err {
                    error: format!("wasm instantiation failed: {e}"),
                };
            }
        };

        // Publish the mailbox now that instantiation succeeded.
        // `try_register_component` re-derives the same name-hashed id
        // (ADR-0029, ADR-0030), so the registered id matches the one
        // `init` already saw — assert that invariant to catch any
        // future drift between the precompute and the registry's
        // internal id derivation. The only realistic way to reach the
        // `Err` arm is a concurrent registration that won the slot
        // between our precompute and this insert; in that race, drop
        // the freshly-built component (firing its `on_drop` hook
        // symmetrically with a normal drop) and surface the
        // NameConflict.
        let registered = match self.registry.try_register_component(&name) {
            Ok(id) => id,
            Err(e) => {
                component.on_drop();
                return LoadResult::Err {
                    error: e.to_string(),
                };
            }
        };
        debug_assert_eq!(
            registered, mailbox,
            "registered mailbox id must match precomputed id from name hash",
        );

        // Issue #403: derive auto-subscriptions from the handler
        // manifest now that the mailbox is registered. The guest SDK
        // no longer fires `subscribe_input` mail during `init` for the
        // six fixed substrate input streams; the substrate wires them
        // directly so the subscribe is observable the moment the
        // component is reachable.
        auto_subscribe_inputs(&self.input_subscribers, mailbox, &capabilities);

        self.insert_component(mailbox, component);
        self.announce_kinds();

        LoadResult::Ok {
            mailbox_id: mailbox,
            name,
            capabilities,
        }
    }

    fn handle_drop(&self, bytes: &[u8]) -> DropResult {
        let payload: DropComponent = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return DropResult::Err { error },
        };
        let id = payload.mailbox_id;
        if let Err(e) = self.registry.drop_mailbox(id) {
            return DropResult::Err {
                error: e.to_string(),
            };
        }
        // ADR-0021 §4: clear this mailbox from every input subscriber
        // set. Done after the registry marks the mailbox `Dropped` so
        // the invariant "every subscriber id references a live mailbox"
        // holds across the short window before the entry is removed
        // from the scheduler table — any mail the platform thread
        // publishes in that window is already discarded by the
        // router's `Dropped` arm, so fan-out to a soon-to-be-empty
        // subscriber set is harmless.
        input::remove_from_all(&self.input_subscribers, id);
        let Some(entry) = self.components.write().unwrap().remove(&id) else {
            return DropResult::Ok;
        };
        // ADR-0038: `close_and_join` drops the entry's `Sender`
        // (closing the inbox), then joins the dispatcher thread. The
        // dispatcher drains any mail already in the inbox before
        // seeing `recv() == None`, so in-flight deliveries to this
        // component complete before the `Component` crosses back to
        // this thread. A stuck wasm `deliver` would hang the join —
        // same failure mode as any blocking scheduler primitive; a
        // bounded-join refinement is follow-up.
        let mut component = close_and_join(entry);
        component.on_drop();
        DropResult::Ok
    }

    fn handle_subscribe(&self, bytes: &[u8]) -> SubscribeInputResult {
        let payload: SubscribeInput = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return SubscribeInputResult::Err { error },
        };
        let id = payload.mailbox;
        if let Err(e) = validate_subscriber_mailbox(&self.registry, id) {
            return SubscribeInputResult::Err { error: e };
        }
        self.input_subscribers
            .write()
            .unwrap()
            .entry(payload.stream)
            .or_default()
            .insert(id);
        SubscribeInputResult::Ok
    }

    fn handle_unsubscribe(&self, bytes: &[u8]) -> SubscribeInputResult {
        let payload: UnsubscribeInput = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return SubscribeInputResult::Err { error },
        };
        let id = payload.mailbox;
        // Unsubscribe is idempotent on "not currently subscribed" but
        // still validates the mailbox: an unknown or sink id is a
        // clear programming error, not something to swallow. A dropped
        // mailbox has already had its subscriptions cleared by
        // handle_drop, but accepting one here would mask a caller bug
        // where they unsubscribe using a stale id. Err is the right
        // answer in both cases.
        if let Err(e) = validate_subscriber_mailbox(&self.registry, id) {
            return SubscribeInputResult::Err { error: e };
        }
        if let Some(set) = self
            .input_subscribers
            .write()
            .unwrap()
            .get_mut(&payload.stream)
        {
            set.remove(&id);
        }
        SubscribeInputResult::Ok
    }

    fn handle_replace(&self, bytes: &[u8]) -> ReplaceResult {
        let payload: ReplaceComponent = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return ReplaceResult::Err { error },
        };
        let id = payload.mailbox_id;
        // ADR-0038 retires the freeze-drain timeout as a load-bearing
        // knob — the splice is structural. The field is still
        // accepted for wire compatibility and ignored here; a future
        // ADR can repurpose it as a join-timeout if stuck guests
        // become common.
        let _drain_timeout_ms = payload.drain_timeout_ms.unwrap_or(DEFAULT_DRAIN_TIMEOUT_MS);

        // Target must be a live Component. Reject unknown ids, sinks,
        // and already-dropped mailboxes before touching wasmtime.
        match self.registry.entry(id) {
            Some(crate::registry::MailboxEntry::Component) => {}
            Some(crate::registry::MailboxEntry::Sink(_)) => {
                return ReplaceResult::Err {
                    error: format!("mailbox {} is a sink, not a component", id.0),
                };
            }
            Some(crate::registry::MailboxEntry::Dropped) => {
                return ReplaceResult::Err {
                    error: format!("mailbox {} already dropped", id.0),
                };
            }
            None => {
                return ReplaceResult::Err {
                    error: format!("unknown mailbox id {}", id.0),
                };
            }
        }

        // ADR-0028: read the kind vocabulary from the wasm's
        // `aether.kinds` custom section; see `handle_load`.
        let descriptors: Vec<KindDescriptor> = match kind_manifest::read_from_bytes(&payload.wasm) {
            Ok(d) => d,
            Err(error) => return ReplaceResult::Err { error },
        };
        if let Err(error) = register_or_match_all(&self.registry, &descriptors) {
            return ReplaceResult::Err { error };
        }

        // ADR-0033: refresh capabilities from the new wasm's
        // `aether.kinds.inputs` section so the hub's cached state
        // tracks the swapped binary (not the pre-replace snapshot).
        let capabilities = match kind_manifest::read_inputs_from_bytes(&payload.wasm) {
            Ok(c) => c,
            Err(error) => return ReplaceResult::Err { error },
        };

        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return ReplaceResult::Err {
                    error: format!("invalid wasm module: {e}"),
                };
            }
        };

        let entry = match self.components.read().unwrap().get(&id).map(Arc::clone) {
            Some(e) => e,
            None => {
                return ReplaceResult::Err {
                    error: format!("mailbox {} has no bound component", id.0),
                };
            }
        };

        // ADR-0022 drain-on-swap invariant, preserved under ADR-0038
        // by the channel splice: `splice_inbox` installs a fresh
        // `(Sender, Receiver)` on `entry`, drops the old `Sender` so
        // the old dispatcher sees `recv() == None` after draining its
        // inbox, and joins the old thread. Mail sent between the
        // splice and the new dispatcher's spawn buffers in the new
        // `Receiver` and reaches the new instance in send order.
        let (mut old_component, new_rx) = crate::scheduler::splice_inbox(&entry);

        // ADR-0015 §3 + ADR-0016 §4: hooks run on the old Component
        // once it's back on this thread. If a save fails we abort and
        // restore: the old Component goes back onto the post-splice
        // inbox so the buffered mail drains through it. `on_drop` has
        // not yet fired on the restoration path.
        old_component.on_replace();
        if let Some(err) = old_component.take_save_error() {
            crate::scheduler::spawn_dispatcher_on(
                &entry,
                old_component,
                new_rx,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
            );
            return ReplaceResult::Err { error: err };
        }
        let saved = old_component.take_saved_state();
        old_component.on_drop();

        // ADR-0042: under the post-amendment drain+buffer design,
        // the replacement inherits nothing wait-related. `spawn_
        // dispatcher_on` installs the post-splice `Receiver` onto
        // the fresh ctx; the old instance's overflow buffer goes
        // away with it.
        let ctx = SubstrateCtx::new(
            id,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            Arc::clone(&self.outbound),
            Arc::clone(&self.input_subscribers),
        );
        let mut new_component =
            match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
                Ok(c) => c,
                Err(e) => {
                    // Restore the old Component onto the post-splice
                    // inbox so buffered mail isn't lost. ADR-0015
                    // wart: on_drop already fired on the old
                    // instance; mail delivered here runs against a
                    // torn-down Component.
                    crate::scheduler::spawn_dispatcher_on(
                        &entry,
                        old_component,
                        new_rx,
                        Arc::clone(&self.registry),
                        Arc::clone(&self.queue),
                    );
                    return ReplaceResult::Err {
                        error: format!("wasm instantiation failed: {e}"),
                    };
                }
            };

        // ADR-0016 §4 step 5: rehydrate the new instance if the old
        // one produced a bundle. A trap or memory-write failure here
        // aborts the replace with the same ADR-0015 wart as the
        // instantiation-fail path above.
        if let Some(bundle) = saved
            && let Err(e) = new_component.call_on_rehydrate(&bundle)
        {
            crate::scheduler::spawn_dispatcher_on(
                &entry,
                old_component,
                new_rx,
                Arc::clone(&self.registry),
                Arc::clone(&self.queue),
            );
            return ReplaceResult::Err {
                error: format!("on_rehydrate failed: {e}"),
            };
        }

        // Commit: spawn a new dispatcher for the new Component onto
        // the post-splice inbox. Buffered mail drains through the new
        // instance in send order.
        drop(old_component);
        crate::scheduler::spawn_dispatcher_on(
            &entry,
            new_component,
            new_rx,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
        );

        // Issue #403: drive auto-subscriptions substrate-side here
        // too, now that the SDK walker is retired. Replace is
        // additive — existing subscriptions (whether they came from
        // the prior binary's manifest or from a runtime
        // `ctx.subscribe_input::<K>()` call) are preserved (ADR-0021
        // §4: "subscriptions are preserved across replace_component").
        // We only ensure the new manifest's input streams are wired,
        // matching the pre-#403 SDK walker behaviour.
        auto_subscribe_inputs(&self.input_subscribers, id, &capabilities);

        self.announce_kinds();
        ReplaceResult::Ok { capabilities }
    }

    fn insert_component(&self, id: MailboxId, component: Component) {
        let entry = ComponentEntry::spawn(
            component,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            id,
        );
        self.components.write().unwrap().insert(id, Arc::new(entry));
    }

    /// Ship the complete current kind vocabulary to the hub so its
    /// per-engine descriptor cache (ADR-0007) reflects kinds that were
    /// registered at runtime (ADR-0010 §4). Called after a successful
    /// load or replace; drop doesn't affect the vocabulary.
    ///
    /// The substrate is authoritative on what it has registered, so we
    /// send the full list rather than a delta — simpler protocol, no
    /// ordering hazard, trivial on the wire (descriptors are small).
    /// If no hub is attached the outbound silently drops — harmless.
    fn announce_kinds(&self) {
        let kinds = self.registry.list_kind_descriptors();
        self.outbound.send(EngineToHub::KindsChanged(kinds));
    }
}

#[cfg(test)]
mod tests;
