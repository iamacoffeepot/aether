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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_hub_protocol::{EngineToHub, KindDescriptor};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, DropComponent, DropResult, LoadComponent, LoadResult,
    MailEnvelope, PlatformInfo, ReplaceComponent, ReplaceResult, SetWindowMode,
    SetWindowModeResult, SubscribeInput, SubscribeInputResult, UnsubscribeInput,
};
use aether_mail::Kind;
use wasmtime::{Engine, Linker, Module};

use crate::capture::{CaptureQueue, PendingCapture};
use crate::chassis::Chassis;
use crate::component::Component;
use crate::ctx::SubstrateCtx;
use crate::hub_client::HubOutbound;
use crate::input::{self, InputSubscribers};
use crate::kind_manifest;
use crate::mail::{Mail, MailboxId};
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

/// Postcard-decode a control-plane payload with the one error-message
/// shape every handler uses. Handlers wrap the `String` in their own
/// `*Result::Err` variant — the shape is uniform, the enum differs.
fn decode_payload<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("postcard decode failed: {e}"))
}

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
/// caller surfaces it as a `CaptureFrameResult::Err`.
fn resolve_bundle(
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
    pub queue: Arc<MailQueue>,
    pub outbound: Arc<HubOutbound>,
    pub components: ComponentTable,
    /// Handoff slot for `aether.control.capture_frame`. The handler
    /// pushes a pending request here; the render thread pulls it on
    /// the next frame and fulfils the reply. The queue's chassis
    /// handle (for wake-on-request) is the same `Arc<dyn Chassis>`
    /// installed below.
    pub capture_queue: CaptureQueue,
    /// ADR-0035 chassis handle. Core delegates peripheral operations
    /// here: `platform_info` and `set_window_mode` requests hand the
    /// sender off for the chassis to run on its own thread and reply
    /// to. Tests use `NoopChassis`.
    pub chassis: Arc<dyn Chassis>,
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
            self.outbound.send_reply(sender, &result);
        } else if kind_name == DropComponent::NAME {
            let result = self.handle_drop(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_name == ReplaceComponent::NAME {
            let result = self.handle_replace(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_name == SubscribeInput::NAME {
            let result = self.handle_subscribe(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_name == UnsubscribeInput::NAME {
            let result = self.handle_unsubscribe(bytes);
            self.outbound.send_reply(sender, &result);
        } else if kind_name == CaptureFrame::NAME {
            self.handle_capture_frame(sender, bytes);
        } else if kind_name == PlatformInfo::NAME {
            // Empty payload; forward the sender straight to the event
            // loop and let it snapshot + reply on its own thread
            // (winit monitor / scale-factor APIs require it).
            self.chassis.request_platform_info(sender);
        } else if kind_name == SetWindowMode::NAME {
            self.handle_set_window_mode(sender, bytes);
        } else {
            tracing::warn!(
                target: "aether_substrate::control",
                kind = %kind_name,
                "{AETHER_CONTROL} received unrecognised kind — dropping",
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
            Arc::clone(&self.input_subscribers),
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
            capabilities,
        }
    }

    fn handle_drop(&self, bytes: &[u8]) -> DropResult {
        let payload: DropComponent = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return DropResult::Err { error },
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
        let payload: SubscribeInput = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return SubscribeInputResult::Err { error },
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
        let payload: UnsubscribeInput = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return SubscribeInputResult::Err { error },
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

    /// Handler for `aether.control.capture_frame`. Two phases:
    ///
    /// 1. Resolve every envelope in *both* the `mails` (pre-capture)
    ///    and `after_mails` (post-capture cleanup) bundles against
    ///    the registry. If *any* envelope fails, abort the whole
    ///    request: no mail is pushed, no capture is requested, reply
    ///    is `Err`. Atomicity guarantee covers both bundles.
    /// 2. Push every resolved pre-capture mail onto the queue, then
    ///    request the capture with the resolved `after_mails` stashed
    ///    on the `PendingCapture`. The render thread's existing
    ///    `queue.wait_idle()` → `take capture` ordering ensures the
    ///    captured frame reflects the pre-capture bundle's effects;
    ///    after readback, the render thread pushes `after_mails`
    ///    before replying.
    ///
    /// The capture itself happens on the render thread (where the
    /// wgpu device lives), so this handler returns without replying
    /// on the happy path; the render thread fulfils via `outbound`.
    /// `Err` replies (decode failure, envelope-resolve failure,
    /// capture-already-pending) are sent inline.
    fn handle_capture_frame(&self, sender: aether_hub_protocol::SessionToken, bytes: &[u8]) {
        let payload: CaptureFrame = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => {
                self.outbound
                    .send_reply(sender, &CaptureFrameResult::Err { error });
                return;
            }
        };

        // Phase 1: resolve every envelope in both bundles before
        // pushing anything or requesting a capture. Any failure
        // aborts the whole request so a partial dispatch can't leak
        // into the next frame — and so a bad `after_mails` bundle
        // can't slip through after the pre-capture mail has already
        // been queued.
        let pre = match resolve_bundle(&self.registry, &payload.mails, "capture bundle") {
            Ok(v) => v,
            Err(e) => {
                self.outbound
                    .send_reply(sender, &CaptureFrameResult::Err { error: e });
                return;
            }
        };
        let after =
            match resolve_bundle(&self.registry, &payload.after_mails, "capture after bundle") {
                Ok(v) => v,
                Err(e) => {
                    self.outbound
                        .send_reply(sender, &CaptureFrameResult::Err { error: e });
                    return;
                }
            };

        // Phase 2: push every resolved pre-capture mail, then request
        // the capture. Order matters on the wire but workers dispatch
        // concurrently; `queue.wait_idle()` on the render thread is
        // what enforces "capture after all mail processed".
        for mail in pre {
            self.queue.push(mail);
        }

        let pending = PendingCapture {
            sender,
            after_mails: after,
        };
        if !self.capture_queue.request(pending) {
            let result = CaptureFrameResult::Err {
                error: "capture already pending; try again once the in-flight \
                    request completes"
                    .to_owned(),
            };
            self.outbound.send_reply(sender, &result);
        }
        // Else: render thread will reply on its next redraw.
    }

    /// Handler for `aether.control.set_window_mode`. Decodes the
    /// request, then hands it to the chassis via
    /// `chassis.request_set_window_mode` — the chassis resolves video
    /// modes, applies the change, and replies on its own thread.
    /// Decode failures reply inline so the caller doesn't hang on a
    /// malformed body.
    fn handle_set_window_mode(&self, sender: aether_hub_protocol::SessionToken, bytes: &[u8]) {
        let payload: SetWindowMode = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => {
                self.outbound
                    .send_reply(sender, &SetWindowModeResult::Err { error });
                return;
            }
        };
        self.chassis
            .request_set_window_mode(sender, payload.mode, payload.width, payload.height);
    }

    fn handle_replace(&self, bytes: &[u8]) -> ReplaceResult {
        let payload: ReplaceComponent = match decode_payload(bytes) {
            Ok(p) => p,
            Err(error) => return ReplaceResult::Err { error },
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
                    error: format!("mailbox {} has no bound component", id.0),
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
            Arc::clone(&self.input_subscribers),
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
        ReplaceResult::Ok { capabilities }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Mail;
    use aether_hub_protocol::{Primitive, SchemaType, SessionToken};
    use std::sync::atomic::AtomicU32;

    #[test]
    fn load_payload_roundtrip() {
        let p = LoadComponent {
            wasm: vec![0, 1, 2, 3],
            name: Some("hello".into()),
        };
        let bytes = postcard::to_allocvec(&p).unwrap();
        let back: LoadComponent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.wasm, p.wasm);
        assert_eq!(back.name.as_deref(), Some("hello"));
    }

    #[test]
    fn load_result_roundtrip() {
        for r in [
            LoadResult::Ok {
                mailbox_id: 7,
                name: "x".into(),
                capabilities: aether_kinds::ComponentCapabilities::default(),
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
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// WAT with lifecycle hooks. Each hook writes a marker to a
    /// distinct offset in linear memory so tests can observe which
    /// hook fired. `on_replace` writes 0x11 at offset 200;
    /// `on_drop` writes 0x22 at offset 204.
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
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
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
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
            chassis: crate::chassis::noop_chassis(),
        }
    }

    #[test]
    fn load_component_instantiates_and_registers() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).expect("compile WAT");
        let payload = LoadComponent {
            wasm,
            name: Some("loaded".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResult::Ok {
                mailbox_id,
                name,
                capabilities: _,
            } => {
                assert_eq!(name, "loaded");
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
        let payload = LoadComponent { wasm, name: None };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResult::Ok { name, .. } => {
                assert!(name.starts_with("component_"), "got {name:?}");
            }
            LoadResult::Err { error } => panic!("load should succeed: {error}"),
        }
    }

    #[test]
    fn load_component_rejects_name_conflict() {
        let plane = make_plane();
        plane.registry.register_component("taken");
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponent {
            wasm,
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
            name: Some("bad_wasm".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(matches!(result, LoadResult::Err { .. }));
    }

    /// ADR-0028 / ADR-0032 happy path: a component ships both its
    /// canonical `aether.kinds` record and the paired
    /// `aether.kinds.labels` sidecar. The substrate reads both and
    /// registers the named kind before instantiation.
    #[test]
    fn load_component_registers_kinds_from_embedded_manifest() {
        let plane = make_plane();

        // Hand-roll the v0x02 records the derive would emit.
        let shape = aether_hub_protocol::KindShape {
            name: std::borrow::Cow::Borrowed("demo.embedded.kind"),
            schema: aether_hub_protocol::SchemaShape::Struct {
                fields: vec![aether_hub_protocol::SchemaShape::Scalar(Primitive::U32)],
                repr_c: true,
            },
        };
        let labels = aether_hub_protocol::KindLabels {
            kind_label: std::borrow::Cow::Borrowed("demo::EmbeddedKind"),
            root: aether_hub_protocol::LabelNode::Struct {
                type_label: Some(std::borrow::Cow::Borrowed("demo::EmbeddedKind")),
                field_names: std::borrow::Cow::Owned(vec![std::borrow::Cow::Borrowed("code")]),
                fields: std::borrow::Cow::Owned(vec![aether_hub_protocol::LabelNode::Anonymous]),
            },
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let mut labels_bytes = vec![0x02u8];
        labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
        let esc = |bs: &[u8]| -> String { bs.iter().map(|b| format!("\\{b:02x}")).collect() };
        let wat = format!(
            r#"(module
                (@custom "aether.kinds" "{}")
                (@custom "aether.kinds.labels" "{}")
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
                    i32.const 0))"#,
            esc(&canonical),
            esc(&labels_bytes),
        );
        let wasm = wat::parse_str(wat).unwrap();

        let loaded = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                name: Some("embedded_consumer".into()),
            })
            .unwrap(),
        );
        assert!(
            matches!(loaded, LoadResult::Ok { .. }),
            "load result was {loaded:?}",
        );
        let registered_id = plane
            .registry
            .kind_id("demo.embedded.kind")
            .expect("manifest kind registered");
        let back = plane
            .registry
            .kind_descriptor(registered_id)
            .expect("descriptor recoverable");
        let SchemaType::Struct { fields, repr_c } = &back.schema else {
            panic!("expected Struct");
        };
        assert!(*repr_c);
        assert_eq!(fields[0].name, "code");
        assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
    }

    /// Same flow, but the embedded manifest conflicts with a kind
    /// already registered with a different schema. The load aborts
    /// rather than silently clobbering — same contract as the
    /// legacy `LoadKind` conflict path.
    #[test]
    fn load_component_with_same_name_different_schema_registers_distinct_kind() {
        // ADR-0030 Phase 2: kind ids are `fnv1a(canonical(name, schema))`,
        // so two schemas under the same name produce two distinct ids
        // and coexist in the registry. The pre-Phase-2 behavior — load
        // rejected with "already registered with a different encoding"
        // — is gone; what used to be a conflict is now a clean new
        // registration. Producer/consumer parity is defended instead
        // by `K::ID` mismatch on the wire (a stale sender's mail lands
        // on "kind not found" at decode time).
        let plane = make_plane();

        let existing_id = plane
            .registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "demo.conflict".into(),
                schema: SchemaType::Scalar(Primitive::U32),
            })
            .unwrap();

        let shape = aether_hub_protocol::KindShape {
            name: std::borrow::Cow::Borrowed("demo.conflict"),
            schema: aether_hub_protocol::SchemaShape::Scalar(Primitive::U64),
        };
        let labels = aether_hub_protocol::KindLabels {
            kind_label: std::borrow::Cow::Borrowed("demo::Conflict"),
            root: aether_hub_protocol::LabelNode::Anonymous,
        };
        let mut canonical = vec![0x02u8];
        canonical.extend(postcard::to_allocvec(&shape).unwrap());
        let mut labels_bytes = vec![0x02u8];
        labels_bytes.extend(postcard::to_allocvec(&labels).unwrap());
        let esc = |bs: &[u8]| -> String { bs.iter().map(|b| format!("\\{b:02x}")).collect() };
        let wat = format!(
            r#"(module
                (@custom "aether.kinds" "{}")
                (@custom "aether.kinds.labels" "{}")
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
                    i32.const 0))"#,
            esc(&canonical),
            esc(&labels_bytes),
        );
        let wasm = wat::parse_str(wat).unwrap();

        let result = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
                name: Some("conflict_consumer".into()),
            })
            .unwrap(),
        );
        assert!(
            matches!(result, LoadResult::Ok { .. }),
            "load should succeed under hashed ids, got {result:?}"
        );

        let new_id = aether_hub_protocol::canonical::kind_id_from_parts(
            "demo.conflict",
            &SchemaType::Scalar(Primitive::U64),
        );
        assert_ne!(
            existing_id, new_id,
            "u32 and u64 schemas under the same name must hash to distinct ids"
        );
        assert_eq!(
            plane.registry.kind_descriptor(existing_id).unwrap().schema,
            SchemaType::Scalar(Primitive::U32)
        );
        assert_eq!(
            plane.registry.kind_descriptor(new_id).unwrap().schema,
            SchemaType::Scalar(Primitive::U64)
        );
    }

    #[test]
    fn drop_component_removes_component_and_frees_name() {
        let plane = make_plane();
        // Load first, then drop the same mailbox.
        let wasm = wat::parse_str(WAT).unwrap();
        let loaded = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
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
        let LoadResult::Ok {
            mailbox_id,
            name,
            capabilities: _,
        } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm: wasm.clone(),
                name: Some("swap_target".into()),
            })
            .unwrap(),
        )
        else {
            panic!("load should succeed");
        };
        assert_eq!(name, "swap_target");

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id,
                wasm,
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }));
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
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }));
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
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");

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
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");
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
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }));
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

    fn load_blank(plane: &ControlPlane, name: &str) -> u64 {
        let wasm = wat::parse_str(WAT).unwrap();
        let result = plane.handle_load(
            &postcard::to_allocvec(&LoadComponent {
                wasm,
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
        mailbox: u64,
        stream: InputStream,
    ) -> SubscribeInputResult {
        plane.handle_subscribe(&postcard::to_allocvec(&SubscribeInput { stream, mailbox }).unwrap())
    }

    fn do_unsubscribe(
        plane: &ControlPlane,
        mailbox: u64,
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
                drain_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }));
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
                payload: sink_id.0.to_le_bytes().to_vec(),
                count: n,
                sender: SessionToken::NIL,
                from_component: None,
            });
        }

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
                drain_timeout_ms: Some(500),
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResult::Ok { .. }), "got {result:?}");

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
                payload: sink_id.0.to_le_bytes().to_vec(),
                count: n,
                sender: SessionToken::NIL,
                from_component: None,
            });
        }

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponent {
                mailbox_id: id,
                wasm: wat::parse_str(WAT_FORWARDS_TO_SINK).unwrap(),
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
    /// the sink mailbox encoded in the payload's first 8 bytes (as
    /// a little-endian u64 — ADR-0029 made mailbox ids 64-bit name
    /// hashes, so 1-byte truncation no longer works). Used by the
    /// drain-flush tests so we can observe whether the new (or old)
    /// instance handled each parked mail.
    const WAT_FORWARDS_TO_SINK: &str = r#"
        (module
            (import "aether" "send_mail_p32"
                (func $send_mail (param i64 i64 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "receive_p32")
                (param $kind i64) (param $ptr i32) (param $count i32) (param $sender i32)
                (result i32)
                (drop (call $send_mail
                    (i64.load (local.get $ptr))
                    (i64.const 0)
                    (i32.const 0)
                    (i32.const 0)
                    (local.get $count)))
                i32.const 0))
    "#;
}
