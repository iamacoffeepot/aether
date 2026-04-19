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
// directly via postcard, converts the runtime-loaded kind list
// (`LoadKind`) to `KindDescriptor`s for the registry, and replies
// with the matching result kind.
//
// Error discipline: agent-visible failures (bad postcard, kind
// conflict, name conflict, invalid WASM, wasmtime instantiation
// error, unknown/wrong-type mailbox) surface as an `Err` variant on
// the matching result. Panics are reserved for invariant violations
// that the agent cannot have caused — e.g. a poisoned lock.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_hub_protocol::{
    ClaudeAddress, EngineMailFrame, EngineToHub, KindDescriptor, NamedField, Primitive, SchemaType,
};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, DropComponent, DropResult, LoadComponent, LoadKind,
    LoadKindEncoding, LoadKindPrimitive, LoadResult, ReplaceComponent, ReplaceResult,
    SubscribeInput, SubscribeInputResult, UnsubscribeInput,
};
use aether_mail::Kind;
use serde::Serialize;
use wasmtime::{Engine, Linker, Module};

use crate::capture::CaptureQueue;
use crate::component::Component;
use crate::ctx::SubstrateCtx;
use crate::hub_client::HubOutbound;
use crate::input::{self, InputSubscribers};
use crate::mail::MailboxId;
use crate::queue::MailQueue;
use crate::registry::{Registry, SinkHandler};
use crate::scheduler::ComponentTable;

/// Well-known mailbox name for the ADR-0010 control plane. Mail to
/// this name is routed to the control-plane sink handler rather than
/// a component. Kept as a constant so substrate init, tests, and any
/// future tooling share one spelling.
pub const AETHER_CONTROL: &str = "aether.control";

/// ADR-0022 default ceiling on the freeze-drain phase of
/// `replace_component`. Per-replace overridable via
/// `ReplaceComponent::drain_timeout_ms`.
pub const DEFAULT_DRAIN_TIMEOUT_MS: u32 = 5_000;

/// Spin-sleep cadence for `drain_pending`. Short enough that the
/// usual sub-millisecond drain returns within a single sleep, long
/// enough that the polling thread doesn't burn a core when a
/// component has a slow `deliver`.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_micros(200);

/// Block until the entry's `pending` count reaches zero or `timeout`
/// elapses. Returns `true` if the drain completed, `false` on
/// timeout. Polled rather than condvar-driven to keep
/// `ComponentEntry` lock-free on the hot dispatch path.
fn drain_pending(entry: &crate::scheduler::ComponentEntry, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if entry.pending.load(Ordering::Acquire) == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return entry.pending.load(Ordering::Acquire) == 0;
        }
        std::thread::sleep(DRAIN_POLL_INTERVAL);
    }
}

/// Deliver every parked mail through `target`. The caller must hold
/// the components-table write lock so workers can't dispatch
/// concurrently to either `entry.component` or `target` — that's
/// what makes the per-component serialization invariant hold across
/// the flush. Parked mail was already counted off the shared
/// queue's outstanding tally when it was parked (see
/// `worker_loop`), so we don't touch it here.
fn flush_parked_to(
    entry: &crate::scheduler::ComponentEntry,
    target: &Mutex<Component>,
    _queue: &MailQueue,
) {
    let mut parked = entry.parked.lock().unwrap();
    if parked.is_empty() {
        return;
    }
    let mut comp = target.lock().unwrap();
    while let Some(mail) = parked.pop_front() {
        comp.deliver(&mail).expect("component.deliver failed");
    }
}

/// Translate a `LoadKind` (the flat, agent-shippable descriptor) into
/// the recursive `KindDescriptor` the registry stores. `Signal`
/// becomes `Schema(Unit)`; `Pod` becomes `Schema(Struct{repr_c:true,
/// fields:[...]})` — same wire format as PR 4's substrate-mail
/// kinds, so a runtime-registered kind is indistinguishable from a
/// boot-registered one on the wire.
fn lift_load_kind(k: &LoadKind) -> KindDescriptor {
    let schema = match &k.encoding {
        LoadKindEncoding::Signal => SchemaType::Unit,
        LoadKindEncoding::Pod { fields } => {
            let named = fields
                .iter()
                .map(|f| NamedField {
                    name: f.name.clone(),
                    ty: lift_load_field_type(f.primitive, f.array_len),
                })
                .collect();
            SchemaType::Struct {
                fields: named,
                repr_c: true,
            }
        }
    };
    KindDescriptor {
        name: k.name.clone(),
        schema,
    }
}

fn lift_load_field_type(primitive: LoadKindPrimitive, array_len: Option<u32>) -> SchemaType {
    let scalar = SchemaType::Scalar(lift_primitive(primitive));
    match array_len {
        None => scalar,
        Some(len) => SchemaType::Array {
            element: Box::new(scalar),
            len,
        },
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

fn lift_primitive(p: LoadKindPrimitive) -> Primitive {
    match p {
        LoadKindPrimitive::U8 => Primitive::U8,
        LoadKindPrimitive::U16 => Primitive::U16,
        LoadKindPrimitive::U32 => Primitive::U32,
        LoadKindPrimitive::U64 => Primitive::U64,
        LoadKindPrimitive::I8 => Primitive::I8,
        LoadKindPrimitive::I16 => Primitive::I16,
        LoadKindPrimitive::I32 => Primitive::I32,
        LoadKindPrimitive::I64 => Primitive::I64,
        LoadKindPrimitive::F32 => Primitive::F32,
        LoadKindPrimitive::F64 => Primitive::F64,
    }
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
    pub queue: Arc<MailQueue>,
    pub outbound: Arc<HubOutbound>,
    pub components: ComponentTable,
    /// Handoff slot for `aether.control.capture_frame`. The handler
    /// pushes a pending request here; the render thread pulls it on
    /// the next frame and fulfils the reply.
    pub capture_queue: CaptureQueue,
    /// ADR-0021 per-stream subscriber sets, shared with the platform
    /// thread. The control plane mutates this table on subscribe /
    /// unsubscribe / drop; the platform thread reads it to fan out
    /// each published event.
    pub input_subscribers: InputSubscribers,
    /// Monotonic counter for default component names. Only consulted
    /// when the load payload omits `name`.
    pub default_name_counter: Arc<AtomicU64>,
}

impl ControlPlane {
    /// Build the sink handler that should be registered against the
    /// `AETHER_CONTROL` mailbox. The returned `SinkHandler` is
    /// `Send + Sync`; it captures `self` by value (through `Arc`s) so
    /// the caller can discard the `ControlPlane` after registration.
    pub fn into_sink_handler(self) -> SinkHandler {
        Arc::new(
            move |kind_name: &str,
                  _origin: Option<&str>,
                  sender: aether_hub_protocol::SessionToken,
                  bytes: &[u8],
                  _count: u32| {
                self.dispatch(kind_name, sender, bytes);
            },
        )
    }

    fn dispatch(&self, kind_name: &str, sender: aether_hub_protocol::SessionToken, bytes: &[u8]) {
        if kind_name == LoadComponent::NAME {
            let result = self.handle_load(bytes);
            self.reply(sender, LoadResult::NAME, &result);
        } else if kind_name == DropComponent::NAME {
            let result = self.handle_drop(bytes);
            self.reply(sender, DropResult::NAME, &result);
        } else if kind_name == ReplaceComponent::NAME {
            let result = self.handle_replace(bytes);
            self.reply(sender, ReplaceResult::NAME, &result);
        } else if kind_name == SubscribeInput::NAME {
            let result = self.handle_subscribe(bytes);
            self.reply(sender, SubscribeInputResult::NAME, &result);
        } else if kind_name == UnsubscribeInput::NAME {
            let result = self.handle_unsubscribe(bytes);
            self.reply(sender, SubscribeInputResult::NAME, &result);
        } else if kind_name == CaptureFrame::NAME {
            self.handle_capture_frame(sender, bytes);
        } else {
            tracing::warn!(
                target: "aether_substrate::control",
                kind = %kind_name,
                "{AETHER_CONTROL} received unrecognised kind — dropping",
            );
        }
    }

    fn handle_load(&self, bytes: &[u8]) -> LoadResult {
        let payload: LoadComponent = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return LoadResult::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };

        // Kind descriptors first: convert the agent's flat `LoadKind`
        // entries into full `KindDescriptor`s, then pre-check for
        // conflicts. Aborting before allocating a mailbox or compiling
        // WASM means a partial-registration of kinds can't leak.
        let descriptors: Vec<KindDescriptor> = payload.kinds.iter().map(lift_load_kind).collect();
        for kind in &descriptors {
            if let Some(id) = self.registry.kind_id(&kind.name)
                && let Some(existing) = self.registry.kind_descriptor(id)
                && existing.schema != kind.schema
            {
                return LoadResult::Err {
                    error: format!(
                        "kind {:?} already registered with a different encoding",
                        kind.name
                    ),
                };
            }
        }
        for kind in descriptors {
            // Pre-validated above; the only way this can still fail is
            // a concurrent registration, which today doesn't exist (all
            // descriptor-bearing registrations go through here or the
            // single init path). Panic on violation of that invariant
            // rather than surfacing an internal race as a user error.
            self.registry
                .register_kind_with_descriptor(kind)
                .expect("pre-validated; no concurrent descriptor registrations");
        }

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

        let mailbox = match self.registry.try_register_component(&name) {
            Ok(id) => id,
            Err(e) => {
                return LoadResult::Err {
                    error: e.to_string(),
                };
            }
        };

        let ctx = SubstrateCtx::new(
            mailbox,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            Arc::clone(&self.outbound),
        );
        let component = match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
            Ok(c) => c,
            Err(e) => {
                // The mailbox and kinds are left in the registry. A
                // retry with a different name will get a fresh mailbox;
                // the kinds are idempotent and re-registering them is
                // a no-op. Rolling back the mailbox would need a
                // Registry API we don't have yet and is parked.
                return LoadResult::Err {
                    error: format!("wasm instantiation failed: {e}"),
                };
            }
        };

        self.insert_component(mailbox, component);
        self.announce_kinds();

        LoadResult::Ok {
            mailbox_id: mailbox.0,
            name,
        }
    }

    fn handle_drop(&self, bytes: &[u8]) -> DropResult {
        let payload: DropComponent = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return DropResult::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox_id);
        if let Err(e) = self.registry.drop_mailbox(id) {
            return DropResult::Err {
                error: e.to_string(),
            };
        }
        // ADR-0021 §4: clear this mailbox from every input subscriber
        // set. Done after the registry marks the mailbox `Dropped` so
        // the invariant "every subscriber id references a live mailbox"
        // holds across the short window before `remove_component`
        // finishes — any mail the platform thread publishes in that
        // window is already discarded by the scheduler's `Dropped`
        // arm, so fan-out to a soon-to-be-empty subscriber set is
        // harmless.
        input::remove_from_all(&self.input_subscribers, id);
        // Pull the Component out of the scheduler table, fire the
        // ADR-0015 `on_drop` hook on it, then let it drop at end of
        // scope so wasmtime reclaims linear memory. The mailbox was
        // already marked `Dropped` above, so any mail racing in
        // parallel will be discarded regardless of when the hook
        // runs.
        if let Some(mut component) = self.remove_component(id) {
            component.on_drop();
        }
        DropResult::Ok
    }

    fn handle_subscribe(&self, bytes: &[u8]) -> SubscribeInputResult {
        let payload: SubscribeInput = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return SubscribeInputResult::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox);
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
        let payload: UnsubscribeInput = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return SubscribeInputResult::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox);
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

    /// Handler for `aether.control.capture_frame`. The capture itself
    /// happens on the render thread (where the wgpu device lives), so
    /// this handler queues the request and returns without replying;
    /// the render thread fulfils via `outbound`. If a capture is
    /// already in flight, we reject immediately with an error so the
    /// caller sees a clean failure rather than a hang.
    fn handle_capture_frame(&self, sender: aether_hub_protocol::SessionToken, bytes: &[u8]) {
        // Decode for validation; the payload is an empty struct today
        // but decoding rejects garbage and leaves room for future
        // options without a separate versioning mechanism.
        if let Err(e) = postcard::from_bytes::<CaptureFrame>(bytes) {
            let result = CaptureFrameResult::Err {
                error: format!("postcard decode failed: {e}"),
            };
            self.reply(sender, CaptureFrameResult::NAME, &result);
            return;
        }

        if !self.capture_queue.request(sender) {
            let result = CaptureFrameResult::Err {
                error: "capture already pending; try again once the in-flight \
                    request completes"
                    .to_owned(),
            };
            self.reply(sender, CaptureFrameResult::NAME, &result);
        }
        // Else: render thread will reply on its next redraw.
    }

    fn handle_replace(&self, bytes: &[u8]) -> ReplaceResult {
        let payload: ReplaceComponent = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return ReplaceResult::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox_id);
        let drain_timeout = Duration::from_millis(
            payload.drain_timeout_ms.unwrap_or(DEFAULT_DRAIN_TIMEOUT_MS) as u64,
        );

        // Target must be a live Component. Reject unknown ids, sinks,
        // and already-dropped mailboxes before touching wasmtime.
        match self.registry.entry(id) {
            Some(crate::registry::MailboxEntry::Component) => {}
            Some(crate::registry::MailboxEntry::Sink(_)) => {
                return ReplaceResult::Err {
                    error: format!("mailbox {:?} is a sink, not a component", id),
                };
            }
            Some(crate::registry::MailboxEntry::Dropped) => {
                return ReplaceResult::Err {
                    error: format!("mailbox {:?} already dropped", id),
                };
            }
            None => {
                return ReplaceResult::Err {
                    error: format!("unknown mailbox id {:?}", id),
                };
            }
        }

        // Kind descriptors: pre-validate like load_component.
        let descriptors: Vec<KindDescriptor> = payload.kinds.iter().map(lift_load_kind).collect();
        for kind in &descriptors {
            if let Some(kid) = self.registry.kind_id(&kind.name)
                && let Some(existing) = self.registry.kind_descriptor(kid)
                && existing.schema != kind.schema
            {
                return ReplaceResult::Err {
                    error: format!(
                        "kind {:?} already registered with a different encoding",
                        kind.name
                    ),
                };
            }
        }
        for kind in descriptors {
            self.registry
                .register_kind_with_descriptor(kind)
                .expect("pre-validated; no concurrent descriptor registrations");
        }

        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return ReplaceResult::Err {
                    error: format!("invalid wasm module: {e}"),
                };
            }
        };

        // ADR-0022 freeze-drain: clone the Arc out of the table under
        // a brief read lock so we can flip `frozen` and poll `pending`
        // without holding any table-level lock. New mail that arrives
        // during the freeze parks on this entry's `parked` deque;
        // workers finishing in-flight `deliver` calls drive `pending`
        // to zero.
        let old_entry = match self.components.read().unwrap().get(&id).map(Arc::clone) {
            Some(e) => e,
            None => {
                // Registered as a Component above but no entry bound
                // — happens if instantiation lost the race with a
                // concurrent drop. Treat as a stale id.
                return ReplaceResult::Err {
                    error: format!("mailbox {:?} has no bound component", id),
                };
            }
        };
        old_entry.frozen.store(true, Ordering::Release);
        if !drain_pending(&old_entry, drain_timeout) {
            // Drain timeout: old instance stays bound. Unfreeze and
            // flush parked through the old component so accumulated
            // mail isn't dropped on the floor. Holding the write lock
            // here keeps workers from racing on the parked flush.
            let table = self.components.write().unwrap();
            flush_parked_to(&old_entry, &old_entry.component, &self.queue);
            old_entry.frozen.store(false, Ordering::Release);
            drop(table);
            return ReplaceResult::Err {
                error: format!(
                    "drain timeout after {}ms; old instance still bound",
                    drain_timeout.as_millis()
                ),
            };
        }

        // ADR-0015 §3 + ADR-0016 §4: hooks run on the old instance
        // under the write lock before instantiation. Take the lock
        // now, invoke hooks, extract any saved state, then keep the
        // lock while we instantiate + rehydrate + swap so no mail
        // races in. Wart named in ADR-0015: if instantiation below
        // fails, `on_drop` will have already fired on the old
        // instance even though it stays live.
        let mut table = self.components.write().unwrap();
        let mut old_component = old_entry.component.lock().unwrap();
        old_component.on_replace();
        // ADR-0016 §4 step 2: save failures abort the replace
        // before `on_drop` fires, so the old instance is still
        // fully alive. Check the error slot before continuing.
        if let Some(err) = old_component.take_save_error() {
            drop(old_component);
            flush_parked_to(&old_entry, &old_entry.component, &self.queue);
            old_entry.frozen.store(false, Ordering::Release);
            drop(table);
            return ReplaceResult::Err { error: err };
        }
        let saved = old_component.take_saved_state();
        old_component.on_drop();
        drop(old_component);

        let ctx = SubstrateCtx::new(
            id,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            Arc::clone(&self.outbound),
        );
        let mut new_component =
            match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
                Ok(c) => c,
                Err(e) => {
                    // Registry is left as-is; any newly registered
                    // kinds stay. The old component is still bound
                    // (on_replace + on_drop already fired — see wart
                    // above); the bundle is discarded. Parked mail
                    // flushes through the still-bound old instance.
                    flush_parked_to(&old_entry, &old_entry.component, &self.queue);
                    old_entry.frozen.store(false, Ordering::Release);
                    drop(table);
                    return ReplaceResult::Err {
                        error: format!("wasm instantiation failed: {e}"),
                    };
                }
            };

        // ADR-0016 §4 step 5: rehydrate the new instance if the old
        // one produced a bundle. A trap or memory-write failure here
        // aborts the replace: drop the new instance, keep the old
        // in the table. `on_drop` on the old already fired — that's
        // the documented ordering wart.
        if let Some(bundle) = saved
            && let Err(e) = new_component.call_on_rehydrate(&bundle)
        {
            flush_parked_to(&old_entry, &old_entry.component, &self.queue);
            old_entry.frozen.store(false, Ordering::Release);
            drop(table);
            return ReplaceResult::Err {
                error: format!("on_rehydrate failed: {e}"),
            };
        }

        // Build the new entry. Frozen defaults to false so workers
        // dispatch normally as soon as the table swap is visible.
        let new_entry = Arc::new(crate::scheduler::ComponentEntry::new(new_component));
        // ADR-0022 §3: parked mail collected during the freeze flushes
        // to the new instance before the table flips, so it's
        // delivered before any post-swap mail and the agent's
        // happens-before edge holds.
        flush_parked_to(&old_entry, &new_entry.component, &self.queue);
        table.insert(id, Arc::clone(&new_entry));
        drop(table);
        // The old `Arc<ComponentEntry>` (and its wasmtime instance)
        // drops when `old_entry` falls out of scope at function exit.

        self.announce_kinds();
        ReplaceResult::Ok
    }

    fn insert_component(&self, id: MailboxId, component: Component) {
        self.components.write().unwrap().insert(
            id,
            Arc::new(crate::scheduler::ComponentEntry::new(component)),
        );
    }

    fn remove_component(&self, id: MailboxId) -> Option<Component> {
        let entry = self.components.write().unwrap().remove(&id)?;
        Some(crate::scheduler::extract_component(entry))
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

    fn reply<T: Serialize>(
        &self,
        sender: aether_hub_protocol::SessionToken,
        kind_name: &str,
        result: &T,
    ) {
        let payload = match postcard::to_allocvec(result) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(target: "aether_substrate::control", kind = %kind_name, error = %e, "result encode failed");
                return;
            }
        };
        self.outbound.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(sender),
            kind_name: kind_name.to_owned(),
            payload,
            origin: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Mail;
    use aether_hub_protocol::SessionToken;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn load_payload_roundtrip() {
        let p = LoadComponent {
            wasm: vec![0, 1, 2, 3],
            kinds: vec![LoadKind {
                name: "hello.foo".into(),
                encoding: LoadKindEncoding::Signal,
            }],
            name: Some("hello".into()),
        };
        let bytes = postcard::to_allocvec(&p).unwrap();
        let back: LoadComponent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.wasm, p.wasm);
        assert_eq!(back.name.as_deref(), Some("hello"));
        assert_eq!(back.kinds.len(), 1);
    }

    #[test]
    fn load_result_roundtrip() {
        for r in [
            LoadResult::Ok {
                mailbox_id: 7,
                name: "x".into(),
            },
            LoadResult::Err {
                error: "nope".into(),
            },
        ] {
            let bytes = postcard::to_allocvec(&r).unwrap();
            let _back: LoadResult = postcard::from_bytes(&bytes).unwrap();
        }
    }

    /// Minimal WAT module satisfying the substrate's component
    /// contract: exports `memory`, a `receive(i32,i32,i32,i32) -> i32`
    /// that returns 0, and no `init`.
    const WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// WAT with lifecycle hooks. Each hook writes a marker to a
    /// distinct offset in linear memory so tests can observe which
    /// hook fired. `on_replace` writes 0x11 at offset 200;
    /// `on_drop` writes 0x22 at offset 204.
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0)
            (func (export "on_drop") (result i32)
                i32.const 204
                i32.const 0x22
                i32.store
                i32.const 0))
    "#;

    /// WAT where `on_drop` traps via `unreachable`. Used to verify
    /// that a panicking hook does not stall teardown.
    const WAT_TRAPS_ON_DROP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_drop") (result i32)
                unreachable))
    "#;

    /// ADR-0016 save side: `on_replace` saves 4 bytes of 0xDEADBEEF
    /// with schema version 7.
    const WAT_SAVES_STATE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 300) "\de\ad\be\ef")
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 7)
                    (i32.const 300)
                    (i32.const 4)))
                i32.const 0))
    "#;

    /// ADR-0016 save side: attempts a 2 MiB save, which the substrate
    /// rejects over the 1 MiB cap. `save_state` returns status 3 and
    /// the ctx error slot is populated.
    const WAT_SAVES_TOO_LARGE: &str = r#"
        (module
            (import "aether" "save_state_p32"
                (func $save_state (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                (drop (call $save_state
                    (i32.const 1)
                    (i32.const 0)
                    (i32.const 0x00200000)))
                i32.const 0))
    "#;

    /// ADR-0016 load side: `on_rehydrate` copies the bundle bytes to
    /// offset 400 and stores the version at offset 396. Used to prove
    /// migration end-to-end when paired with `WAT_SAVES_STATE`.
    const WAT_REHYDRATES: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i32 i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_rehydrate_p32") (param i32 i32 i32) (result i32)
                i32.const 396
                local.get 0
                i32.store
                i32.const 400
                local.get 1
                local.get 2
                memory.copy
                i32.const 0))
    "#;

    fn make_plane() -> ControlPlane {
        let engine = Arc::new(Engine::default());
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let registry = Arc::new(Registry::new());
        let queue = Arc::new(MailQueue::new());
        let outbound = HubOutbound::disconnected();
        let components: ComponentTable = Arc::default();

        ControlPlane {
            engine,
            linker: Arc::new(linker),
            registry,
            queue,
            outbound,
            components,
            input_subscribers: input::new_subscribers(),
            default_name_counter: Arc::new(AtomicU64::new(0)),
            capture_queue: CaptureQueue::new(),
        }
    }

    #[test]
    fn load_component_instantiates_and_registers() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).expect("compile WAT");
        let payload = LoadComponent {
            wasm,
            kinds: vec![LoadKind {
                name: "loaded.ping".into(),
                encoding: LoadKindEncoding::Signal,
            }],
            name: Some("loaded".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResult::Ok { mailbox_id, name } => {
                assert_eq!(name, "loaded");
                assert!(plane.registry.kind_id("loaded.ping").is_some());
                assert_eq!(plane.registry.lookup("loaded"), Some(MailboxId(mailbox_id)));
                assert!(
                    plane
                        .components
                        .read()
                        .unwrap()
                        .contains_key(&MailboxId(mailbox_id))
                );
            }
            LoadResult::Err { error } => panic!("load should succeed: {error}"),
        }
    }

    #[test]
    fn load_component_defaults_name_on_absent() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponent {
            wasm,
            kinds: vec![],
            name: None,
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResult::Ok { name, .. } => {
                assert!(name.starts_with("component_"), "got {name:?}");
            }
            LoadResult::Err { error } => panic!("load should succeed: {error}"),
        }
    }

    #[test]
    fn load_component_rejects_kind_conflict() {
        let plane = make_plane();
        // Pre-register "shared" as a Pod struct via the descriptor
        // path. The load below requests it as a Signal — different
        // schema arm, so the conflict check rejects.
        plane
            .registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "shared".into(),
                schema: SchemaType::Struct {
                    fields: vec![NamedField {
                        name: "n".into(),
                        ty: SchemaType::Scalar(Primitive::U32),
                    }],
                    repr_c: true,
                },
            })
            .unwrap();
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponent {
            wasm,
            kinds: vec![LoadKind {
                name: "shared".into(),
                encoding: LoadKindEncoding::Signal,
            }],
            name: Some("conflict_case".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(
            matches!(result, LoadResult::Err { .. }),
            "expected conflict error, got {result:?}"
        );
        // Mailbox not allocated on conflict.
        assert!(plane.registry.lookup("conflict_case").is_none());
    }

    #[test]
    fn load_component_rejects_name_conflict() {
        let plane = make_plane();
        plane.registry.register_component("taken");
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponent {
            wasm,
            kinds: vec![],
            name: Some("taken".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(matches!(result, LoadResult::Err { .. }));
    }

    #[test]
    fn load_component_rejects_invalid_wasm() {
        let plane = make_plane();
        let payload = LoadComponent {
            wasm: vec![0, 1, 2, 3],
            kinds: vec![],
            name: Some("bad_wasm".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(matches!(result, LoadResult::Err { .. }));
    }

    #[test]
    fn drop_component_removes_component_and_frees_name() {
        let plane = make_plane();
        // Load first, then drop the same mailbox.
        let wasm = wat::parse_str(WAT).unwrap();
        let loaded = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some("victim".into()),
            })
            .unwrap(),
        );
        let LoadResult::Ok { mailbox_id, .. } = loaded else {
            panic!("load should succeed");
        };

        let dropped =
            plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResult::Ok));
        assert!(
            plane.registry.lookup("victim").is_none(),
            "name should be released so it can be reused"
        );
        assert!(
            matches!(
                plane.registry.entry(MailboxId(mailbox_id)),
                Some(crate::registry::MailboxEntry::Dropped),
            ),
            "entry should be marked Dropped",
        );
        assert!(
            !plane
                .components
                .read()
                .unwrap()
                .contains_key(&MailboxId(mailbox_id)),
            "component must be removed from scheduler table",
        );
    }

    #[test]
    fn drop_component_rejects_unknown_id() {
        let plane = make_plane();
        let result =
            plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id: 99 }).unwrap());
        assert!(matches!(result, DropResult::Err { .. }));
    }

    #[test]
    fn drop_component_rejects_double_drop() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some("once".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let args = postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap();
        assert!(matches!(plane.handle_drop(&args), DropResult::Ok));
        assert!(matches!(plane.handle_drop(&args), DropResult::Err { .. }));
    }

    #[test]
    fn replace_component_swaps_instance_and_preserves_id() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResult::Ok { mailbox_id, name } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wasm.clone(),
                kinds: vec![],
                name: Some("swap_target".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        assert_eq!(name, "swap_target");

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm,
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok));
        // Name still resolves to the same id; new Component bound.
        assert_eq!(
            plane.registry.lookup("swap_target"),
            Some(MailboxId(mailbox_id))
        );
        assert!(
            plane
                .components
                .read()
                .unwrap()
                .contains_key(&MailboxId(mailbox_id))
        );
    }

    #[test]
    fn replace_component_rejects_unknown_target() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: 99,
                wasm,
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Err { .. }));
    }

    #[test]
    fn replace_component_rejects_dropped_target() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wasm.clone(),
                kinds: vec![],
                name: Some("gone".into()),
            })
            .unwrap(),
        ) else {
            panic!();
        };
        plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm,
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Err { .. }));
    }

    #[test]
    fn replace_component_rejects_invalid_wasm() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some("target".into()),
            })
            .unwrap(),
        ) else {
            panic!();
        };
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: vec![0, 1, 2, 3],
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Err { .. }));
    }

    #[test]
    fn drop_component_with_hooks_completes_ok() {
        // WAT_HOOKS exports on_drop. handle_drop should fire it and
        // complete without error; the marker write is exercised in
        // component::tests::on_drop_invokes_export_and_writes_marker.
        let plane = make_plane();
        let wasm = wat::parse_str(WAT_HOOKS).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some("hooked".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let dropped =
            plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResult::Ok));
    }

    #[test]
    fn drop_component_with_trapping_on_drop_still_ok() {
        // ADR-0015 trap containment: a panicking hook must not stall
        // teardown. The handler logs and returns Ok regardless.
        let plane = make_plane();
        let wasm = wat::parse_str(WAT_TRAPS_ON_DROP).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some("crasher".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let dropped =
            plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResult::Ok));
        // Mailbox still marked Dropped; component still removed.
        assert!(matches!(
            plane.registry.entry(MailboxId(mailbox_id)),
            Some(crate::registry::MailboxEntry::Dropped),
        ));
    }

    #[test]
    fn replace_component_fires_hooks_on_old_instance() {
        // handle_replace takes the write lock, fires on_replace +
        // on_drop on the old component, instantiates the new one,
        // and swaps under the same lock. Success means both hooks
        // completed without stalling the replace.
        let plane = make_plane();
        let wasm_old = wat::parse_str(WAT_HOOKS).unwrap();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wasm_old,
                kinds: vec![],
                name: Some("swap_me".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let wasm_new = wat::parse_str(WAT).unwrap();
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: wasm_new,
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok));
    }

    #[test]
    fn dispatch_unrecognised_kind_is_silent_drop() {
        let plane = make_plane();
        // No panic; no outbound reply. Unknown kind arriving at the
        // control mailbox just logs and moves on.
        plane.dispatch("aether.control.does_not_exist", SessionToken::NIL, &[]);
    }

    #[test]
    fn replace_migrates_state_from_old_to_new() {
        // The full ADR-0016 path: load an old instance that saves on
        // replace, replace with a new instance that rehydrates, and
        // observe that the new instance's memory received the bundle.
        let plane = make_plane();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT_SAVES_STATE).unwrap(),
                kinds: vec![],
                name: Some("stateful".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: wat::parse_str(WAT_REHYDRATES).unwrap(),
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok), "got {result:?}");

        // Peek into the new component's memory — rehydrate should
        // have written version 7 at offset 396 and 0xDEADBEEF at
        // offset 400.
        let table = plane.components.read().unwrap();
        let cell = table.get(&MailboxId(mailbox_id)).expect("present");
        let mut new = cell.component.lock().unwrap();
        assert_eq!(new.read_u32(396), 7);
        assert_eq!(new.read_bytes(400, 4), vec![0xDE, 0xAD, 0xBE, 0xEF],);
    }

    #[test]
    fn replace_aborts_when_save_state_over_cap() {
        // Old instance requests a save larger than 1 MiB; substrate
        // rejects, `handle_replace` surfaces the error, old stays live.
        let plane = make_plane();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT_SAVES_TOO_LARGE).unwrap(),
                kinds: vec![],
                name: Some("greedy".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: wat::parse_str(WAT).unwrap(),
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        let ReplaceResult::Err { error } = result else {
            panic!("expected replace to fail, got {result:?}");
        };
        assert!(error.contains("exceeds"), "got: {error}");
        // Old instance is still bound; name still resolves to its id.
        assert_eq!(plane.registry.lookup("greedy"), Some(MailboxId(mailbox_id)));
        assert!(
            plane
                .components
                .read()
                .unwrap()
                .contains_key(&MailboxId(mailbox_id))
        );
    }

    #[test]
    fn replace_without_rehydrate_hook_discards_bundle() {
        // Old saves, new doesn't implement on_rehydrate — the bundle
        // is silently discarded (ADR-0016 §3). Replace succeeds.
        let plane = make_plane();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT_SAVES_STATE).unwrap(),
                kinds: vec![],
                name: Some("orphan_save".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: wat::parse_str(WAT).unwrap(),
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok), "got {result:?}");
    }

    #[test]
    fn replace_with_no_save_does_not_invoke_rehydrate() {
        // Old doesn't save; new has on_rehydrate but it must not
        // fire — ADR-0016 §3 says rehydrate only runs if a bundle
        // exists. The new instance's rehydrate marker offsets should
        // stay zero.
        let plane = make_plane();
        let LoadResult::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT).unwrap(),
                kinds: vec![],
                name: Some("stateless_old".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm: wat::parse_str(WAT_REHYDRATES).unwrap(),
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok));
        let table = plane.components.read().unwrap();
        let cell = table.get(&MailboxId(mailbox_id)).expect("present");
        let mut new = cell.component.lock().unwrap();
        assert_eq!(new.read_u32(396), 0);
        assert_eq!(new.read_u32(400), 0);
    }

    // ADR-0021: per-stream subscribe / unsubscribe, drop cleanup,
    // replace-preserves-subscriptions. `make_plane` already threads an
    // empty `InputSubscribers` into the handler, so these tests only
    // need to load a component and exercise the subscribe surface.

    use aether_kinds::{InputStream, SubscribeInput, SubscribeInputResult, UnsubscribeInput};

    fn subs(plane: &ControlPlane, stream: InputStream) -> std::collections::BTreeSet<MailboxId> {
        plane
            .input_subscribers
            .read()
            .unwrap()
            .get(&stream)
            .cloned()
            .unwrap_or_default()
    }

    fn load_blank(plane: &ControlPlane, name: &str) -> u32 {
        let wasm = wat::parse_str(WAT).unwrap();
        let result = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                kinds: vec![],
                name: Some(name.into()),
            })
            .unwrap(),
        );
        let LoadResult::Ok { mailbox_id, .. } = result else {
            panic!("load should succeed: {result:?}");
        };
        mailbox_id
    }

    fn do_subscribe(
        plane: &ControlPlane,
        mailbox: u32,
        stream: InputStream,
    ) -> SubscribeInputResult {
        plane.handle_subscribe(&postcard::to_allocvec(&SubscribeInput { stream, mailbox }).unwrap())
    }

    fn do_unsubscribe(
        plane: &ControlPlane,
        mailbox: u32,
        stream: InputStream,
    ) -> SubscribeInputResult {
        plane.handle_unsubscribe(
            &postcard::to_allocvec(&UnsubscribeInput { stream, mailbox }).unwrap(),
        )
    }

    #[test]
    fn subscribe_adds_mailbox_to_stream_set() {
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        assert!(matches!(
            do_subscribe(&plane, id, InputStream::Tick),
            SubscribeInputResult::Ok
        ));
        let set = subs(&plane, InputStream::Tick);
        assert!(set.contains(&MailboxId(id)));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn subscribe_is_idempotent() {
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        for _ in 0..3 {
            assert!(matches!(
                do_subscribe(&plane, id, InputStream::Key),
                SubscribeInputResult::Ok
            ));
        }
        assert_eq!(subs(&plane, InputStream::Key).len(), 1);
    }

    #[test]
    fn subscribe_two_components_fan_out_to_both() {
        let plane = make_plane();
        let a = load_blank(&plane, "a");
        let b = load_blank(&plane, "b");
        assert!(matches!(
            do_subscribe(&plane, a, InputStream::Tick),
            SubscribeInputResult::Ok
        ));
        assert!(matches!(
            do_subscribe(&plane, b, InputStream::Tick),
            SubscribeInputResult::Ok
        ));
        let set = subs(&plane, InputStream::Tick);
        assert_eq!(set.len(), 2);
        assert!(set.contains(&MailboxId(a)));
        assert!(set.contains(&MailboxId(b)));
    }

    #[test]
    fn unsubscribe_removes_from_set() {
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        do_subscribe(&plane, id, InputStream::MouseMove);
        assert!(matches!(
            do_unsubscribe(&plane, id, InputStream::MouseMove),
            SubscribeInputResult::Ok
        ));
        assert!(subs(&plane, InputStream::MouseMove).is_empty());
    }

    #[test]
    fn unsubscribe_not_subscribed_is_ok() {
        // ADR-0021 §2: unsubscribe of a non-subscriber is `Ok`, not
        // `Err`. The mailbox must still be a live component though.
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        assert!(matches!(
            do_unsubscribe(&plane, id, InputStream::Tick),
            SubscribeInputResult::Ok
        ));
    }

    #[test]
    fn subscribe_unknown_mailbox_is_err() {
        let plane = make_plane();
        assert!(matches!(
            do_subscribe(&plane, 9999, InputStream::Tick),
            SubscribeInputResult::Err { .. }
        ));
    }

    #[test]
    fn subscribe_sink_mailbox_is_err() {
        // Sinks are substrate-owned and don't make sense as input
        // subscribers; the handler rejects.
        let plane = make_plane();
        let sink = plane
            .registry
            .register_sink("some.sink", Arc::new(|_, _, _, _, _| {}));
        assert!(matches!(
            do_subscribe(&plane, sink.0, InputStream::Tick),
            SubscribeInputResult::Err { .. }
        ));
    }

    #[test]
    fn subscribe_dropped_mailbox_is_err() {
        let plane = make_plane();
        let id = load_blank(&plane, "victim");
        plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id: id }).unwrap());
        assert!(matches!(
            do_subscribe(&plane, id, InputStream::Tick),
            SubscribeInputResult::Err { .. }
        ));
    }

    #[test]
    fn drop_component_removes_from_every_subscriber_set() {
        // ADR-0021 §4: dropping a component clears its id from every
        // stream's subscriber set, not just the ones currently held.
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        do_subscribe(&plane, id, InputStream::Tick);
        do_subscribe(&plane, id, InputStream::Key);
        do_subscribe(&plane, id, InputStream::MouseButton);
        let dropped =
            plane.handle_drop(&postcard::to_allocvec(&DropComponent { mailbox_id: id }).unwrap());
        assert!(matches!(dropped, DropResult::Ok));
        for s in [
            InputStream::Tick,
            InputStream::Key,
            InputStream::MouseMove,
            InputStream::MouseButton,
        ] {
            assert!(
                !subs(&plane, s).contains(&MailboxId(id)),
                "stream {s:?} still contains dropped id"
            );
        }
    }

    #[test]
    fn replace_component_preserves_subscriptions() {
        // ADR-0021 §4: replace keeps the mailbox id, and subscriptions
        // are keyed by mailbox, so the new instance inherits them.
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        do_subscribe(&plane, id, InputStream::Tick);
        do_subscribe(&plane, id, InputStream::Key);
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT).unwrap(),
                kinds: vec![],
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok));
        assert!(subs(&plane, InputStream::Tick).contains(&MailboxId(id)));
        assert!(subs(&plane, InputStream::Key).contains(&MailboxId(id)));
    }

    #[test]
    fn subscribe_malformed_payload_is_err() {
        let plane = make_plane();
        let result = plane.handle_subscribe(&[0xFF; 4]);
        assert!(matches!(result, SubscribeInputResult::Err { .. }));
    }

    #[test]
    fn subscribe_dispatch_replies_with_result_kind() {
        // Dispatch goes through `dispatch()` so a SubscribeInputResult
        // is sent via reply-to-sender. We can't easily observe the
        // outbound here without a richer fake, but at least confirm
        // the dispatch path doesn't panic on the two kinds.
        let plane = make_plane();
        let id = load_blank(&plane, "listener");
        plane.dispatch(
            SubscribeInput::NAME,
            SessionToken::NIL,
            &postcard::to_allocvec(&SubscribeInput {
                stream: InputStream::Tick,
                mailbox: id,
            })
            .unwrap(),
        );
        plane.dispatch(
            UnsubscribeInput::NAME,
            SessionToken::NIL,
            &postcard::to_allocvec(&UnsubscribeInput {
                stream: InputStream::Tick,
                mailbox: id,
            })
            .unwrap(),
        );
        assert!(!subs(&plane, InputStream::Tick).contains(&MailboxId(id)));
    }

    // ADR-0022 freeze-drain-swap. The first three tests poke
    // `pending` / `parked` directly to exercise the drain and flush
    // paths without spinning up a worker pool — the drain logic is
    // expressed against the entry's atomics, so the entry is the
    // right level to test it at.

    #[test]
    fn drain_pending_returns_true_when_count_drops_in_time() {
        let plane = make_plane();
        let id = load_blank(&plane, "drainable");
        let entry = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        entry.pending.store(2, Ordering::SeqCst);
        let entry_for_drainer = Arc::clone(&entry);
        let drainer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            entry_for_drainer.pending.store(0, Ordering::SeqCst);
        });
        assert!(super::drain_pending(&entry, Duration::from_millis(500)));
        drainer.join().unwrap();
    }

    #[test]
    fn replace_drain_timeout_keeps_old_bound() {
        let plane = make_plane();
        let id = load_blank(&plane, "victim");
        let entry_before = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        // Pin pending above zero so drain never completes within the
        // tight per-replace timeout.
        entry_before.pending.store(1, Ordering::SeqCst);

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT).unwrap(),
                kinds: vec![],
                drain_timeout_ms: Some(20),
            })
            .unwrap(),
        );
        let ReplaceResult::Err { error } = result else {
            panic!("expected timeout, got {result:?}");
        };
        assert!(
            error.contains("drain timeout"),
            "unexpected error message: {error}"
        );

        // Same Arc still bound — no swap happened.
        let entry_after = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&entry_before, &entry_after));
        // Frozen flag cleared so future mail flows through again.
        assert!(!entry_after.frozen.load(Ordering::SeqCst));

        // Reset pending so the entry drops cleanly when the table
        // releases it (no real worker to decrement on our behalf).
        entry_after.pending.store(0, Ordering::SeqCst);
    }

    #[test]
    fn replace_flushes_parked_mail_to_new_instance() {
        // Old + new components both forward `receive` to a counter
        // sink. After parking N mails on the entry and triggering a
        // successful replace, the counter records exactly the parked
        // ticks — proving the new instance is the one that handled
        // them post-swap.
        let plane = make_plane();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_sink = Arc::clone(&counter);
        let sink_id = plane.registry.register_sink(
            "drain-flush-sink",
            Arc::new(move |_kind, _origin, _sender, _bytes, count| {
                counter_for_sink.fetch_add(count, Ordering::SeqCst);
            }),
        );

        let LoadResult::Ok { mailbox_id: id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
                kinds: vec![],
                name: Some("flusher".into()),
            })
            .unwrap(),
        ) else {
            panic!("load failed");
        };

        let entry = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        // Park three mails directly on the entry. Real workers would
        // do this when frozen=true; here we simulate the post-park
        // state without standing up a worker pool. We do NOT touch
        // queue.outstanding — these mails are off-queue from the
        // pool's perspective.
        entry.frozen.store(true, Ordering::SeqCst);
        let kind_id = 0; // sink_id payload is unused; kind is irrelevant.
        let _ = kind_id;
        for n in 1..=3u32 {
            entry.parked.lock().unwrap().push_back(Mail {
                recipient: MailboxId(id),
                kind: 0,
                payload: vec![sink_id.0 as u8],
                count: n,
                sender: SessionToken::NIL,
                from_component: None,
            });
        }

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
                kinds: vec![],
                drain_timeout_ms: Some(500),
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok), "got {result:?}");

        // Three parked ticks (counts 1, 2, 3) flushed to the new
        // instance, which forwarded each to the sink.
        assert_eq!(counter.load(Ordering::SeqCst), 1 + 2 + 3);

        // New entry is bound now, parked is empty, frozen cleared.
        let entry_after = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        assert!(!Arc::ptr_eq(&entry, &entry_after));
        assert!(entry_after.parked.lock().unwrap().is_empty());
        assert!(!entry_after.frozen.load(Ordering::SeqCst));
    }

    #[test]
    fn replace_drain_timeout_flushes_parked_to_old() {
        // Pending stays >0 (a forever in-flight deliver), so the
        // replace times out. Parked mail must still be delivered —
        // through the old instance, since the swap didn't happen.
        let plane = make_plane();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_sink = Arc::clone(&counter);
        let sink_id = plane.registry.register_sink(
            "drain-timeout-sink",
            Arc::new(move |_kind, _origin, _sender, _bytes, count| {
                counter_for_sink.fetch_add(count, Ordering::SeqCst);
            }),
        );

        let LoadResult::Ok { mailbox_id: id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
                kinds: vec![],
                name: Some("survivor".into()),
            })
            .unwrap(),
        ) else {
            panic!("load failed");
        };

        let entry = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        entry.pending.store(1, Ordering::SeqCst);
        entry.frozen.store(true, Ordering::SeqCst);
        for n in 1..=2u32 {
            entry.parked.lock().unwrap().push_back(Mail {
                recipient: MailboxId(id),
                kind: 0,
                payload: vec![sink_id.0 as u8],
                count: n,
                sender: SessionToken::NIL,
                from_component: None,
            });
        }

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
                kinds: vec![],
                drain_timeout_ms: Some(20),
            })
            .unwrap(),
        );
        let ReplaceResult::Err { error } = result else {
            panic!("expected timeout, got {result:?}");
        };
        assert!(error.contains("drain timeout"), "{error}");

        // Old instance handled the parked counts (1 + 2 = 3).
        assert_eq!(counter.load(Ordering::SeqCst), 3);

        // Same entry still bound; parked empty, frozen cleared.
        let entry_after = plane
            .components
            .read()
            .unwrap()
            .get(&MailboxId(id))
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&entry, &entry_after));
        assert!(entry_after.parked.lock().unwrap().is_empty());
        assert!(!entry_after.frozen.load(Ordering::SeqCst));

        // Reset for clean drop.
        entry_after.pending.store(0, Ordering::SeqCst);
    }

    /// Component that, on each `receive`, forwards a `send_mail` to
    /// the sink mailbox encoded in the first payload byte. Used by
    /// the drain-flush tests so we can observe whether the new (or
    /// old) instance handled each parked mail.
    const WAT_FORWARDS_TO_SINK: &str = r#"
        (module
            (import "aether" "send_mail_p32"
                (func $send_mail (param i32 i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32")
                (param $kind i32) (param $ptr i32) (param $count i32) (param $sender i32)
                (result i32)
                (drop (call $send_mail
                    (i32.load8_u (local.get $ptr))
                    (i32.const 0)
                    (i32.const 0)
                    (i32.const 0)
                    (local.get $count)))
                i32.const 0))
    "#;
}
