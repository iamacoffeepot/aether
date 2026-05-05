// Inline router (ADR-0038 Phase 3).
//
// Phase 2 retired the VecDeque + router thread; Phase 3 retires the
// global `outstanding` / `done_cv` barrier too. The per-component
// drain primitive on `ComponentEntry` replaces `wait_idle` — callers
// that previously waited for "all mail in flight" now drain the
// specific mailboxes they care about (or iterate the full components
// table via `scheduler::drain_all`).
//
// What survives: `push(mail)` resolves the recipient inline on the
// caller's thread (sinks run inline; components forward to inbox;
// dropped / unknown warn-drop). The module's only state is the
// `Registry` + `ComponentTable` handles, wired once by
// `Scheduler::new`.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use crate::handle_store::{self, HandleStore, PutError, WalkOutcome};
use crate::mail::{Mail, ReplyTarget, ReplyTo};
use crate::outbound::HubOutbound;
use crate::registry::{MailboxEntry, Registry};
use crate::scheduler::ComponentTable;
use aether_data::{HandleId, KindId};

pub struct Mailer {
    /// Registry handle for resolving recipients on `push`. Wired once
    /// by `Scheduler::new`; expected to be set by the time any mail
    /// can land.
    registry: OnceLock<Arc<Registry>>,
    /// Components table for forwarding into per-component inboxes.
    /// Wired alongside `registry`.
    components: OnceLock<ComponentTable>,
    /// Hub outbound handle. When set and connected, mail to unknown
    /// mailbox ids bubbles up to the hub-substrate (ADR-0037
    /// Phase 1) instead of being warn-dropped locally. Wired by
    /// `SubstrateBoot::build` right after the `Mailer` exists; the
    /// boot holds the only `Arc<HubOutbound>` on the fresh side of
    /// construction. Absent on chassis that skip hub connection
    /// (today: the hub chassis itself), which keeps local warn-drop
    /// semantics intact — the hub is the end of the bubbles-up
    /// line.
    outbound: OnceLock<Arc<HubOutbound>>,
    /// ADR-0045 typed-handle resolver. When wired, `route_mail`
    /// runs each mail through the ref-walker before dispatching;
    /// missing handles park the mail in the store. Optional so
    /// pre-PR-2 test paths (and any chassis that opts out by not
    /// calling `wire_handle_store`) keep the original verbatim-
    /// dispatch behaviour. `SubstrateBoot::build` wires this with
    /// `HandleStore::from_env()` for every chassis.
    handle_store: OnceLock<Arc<HandleStore>>,
}

impl Mailer {
    pub fn new() -> Self {
        Self {
            registry: OnceLock::new(),
            components: OnceLock::new(),
            outbound: OnceLock::new(),
            handle_store: OnceLock::new(),
        }
    }

    /// Wire the registry + components table. Called by `Scheduler::new`
    /// once both exist; panics on re-wiring to surface construction-
    /// order bugs loud rather than silent.
    pub fn wire(&self, registry: Arc<Registry>, components: ComponentTable) {
        self.registry
            .set(registry)
            .unwrap_or_else(|_| panic!("Mailer::wire called twice"));
        self.components
            .set(components)
            .unwrap_or_else(|_| panic!("Mailer::wire called twice"));
    }

    /// Wire the `HubOutbound` so mail to unknown mailbox ids bubbles
    /// up to the hub-substrate (ADR-0037 Phase 1) instead of being
    /// warn-dropped. Called by `SubstrateBoot::build` after the
    /// `HubOutbound` exists; harmless to skip for chassis that are
    /// their own hub (the hub chassis doesn't bubble up to itself).
    pub fn wire_outbound(&self, outbound: Arc<HubOutbound>) {
        self.outbound
            .set(outbound)
            .unwrap_or_else(|_| panic!("Mailer::wire_outbound called twice"));
    }

    /// Wire the ADR-0045 handle store. With a store wired, every
    /// mail's payload walks through the ref-resolver before dispatch
    /// (kinds whose schema contains no `Ref` nodes hit the no-op
    /// fast path). Without a store, dispatch behaves exactly like
    /// the pre-PR-2 path. Called by `SubstrateBoot::build`.
    pub fn wire_handle_store(&self, store: Arc<HandleStore>) {
        self.handle_store
            .set(store)
            .unwrap_or_else(|_| panic!("Mailer::wire_handle_store called twice"));
    }

    /// Borrow the wired `HandleStore`, or `None` if no store was
    /// wired (pre-boot / test path). Read-only handle exposed so
    /// chassis-side handlers (PR 3 host-fn shims) can publish into
    /// the same store the dispatch path resolves against.
    pub fn handle_store(&self) -> Option<&Arc<HandleStore>> {
        self.handle_store.get()
    }

    /// Borrow the wired [`HubOutbound`], or `None` if no outbound was
    /// wired (test paths, or chassis that skip hub connection).
    /// Issue 576: surfaced so the broadcast cap's `init` can grab the
    /// outbound at boot and lift catch-all envelopes through
    /// [`HubOutbound::egress_broadcast`] without the substrate
    /// holding a closure-sink for it.
    pub fn outbound(&self) -> Option<&Arc<HubOutbound>> {
        self.outbound.get()
    }

    /// Hand `mail` to the substrate for dispatch. Sinks run on the
    /// caller thread; component mail forwards into the recipient's
    /// inbox (which bumps the per-entry drain counter); dropped /
    /// unknown recipients warn-and-discard (or bubble up to the hub-
    /// substrate when a `HubOutbound` is connected, per ADR-0037).
    pub fn push(&self, mail: Mail) {
        route_mail(
            mail,
            self.registry.get().expect("Mailer not wired"),
            self.components.get().expect("Mailer not wired"),
            self.outbound.get(),
            self.handle_store.get(),
        );
    }

    /// Publish a resolved handle and re-route every mail that was
    /// parked on it. Each parked mail re-walks against its kind
    /// schema; if the same payload still references a *different*
    /// missing handle, the re-walk parks it on that id, otherwise
    /// dispatch proceeds normally with the spliced-inline payload.
    ///
    /// Used by future host-fn shims (PR 3) and by chassis-level code
    /// that resolves handles synchronously. Returns the `PutError`
    /// from the underlying store on byte-budget / kind-id conflicts;
    /// in those cases parked mail stays parked and the caller decides
    /// how to recover.
    ///
    /// Without a wired store this is a no-op success: chassis that
    /// don't expose handles never park mail in the first place.
    pub fn resolve_handle(
        &self,
        handle: HandleId,
        kind: KindId,
        bytes: Vec<u8>,
    ) -> Result<(), PutError> {
        let Some(store) = self.handle_store.get() else {
            return Ok(());
        };
        store.put(handle, kind, bytes)?;
        let parked = store.take_parked(handle);
        let registry = self.registry.get().expect("Mailer not wired");
        let components = self.components.get().expect("Mailer not wired");
        let outbound = self.outbound.get();
        for mail in parked {
            route_mail(mail, registry, components, outbound, Some(store));
        }
        Ok(())
    }

    /// Route a sink's `*Result` reply to `sender` with a single
    /// encode. `Session` / `EngineMailbox` hand off to the hub
    /// outbound (unchanged hub-wire format); `Component` pushes a
    /// fresh `Mail` into the target component's inbox so the guest's
    /// normal dispatch path delivers the reply. `None` is a silent
    /// drop — nobody asked for a reply.
    ///
    /// The reply mail carries `reply_to = None` and no origin: the
    /// receiver isn't expected to reply to a reply, and decorating
    /// with the sink's mailbox would produce a `ReplyEntry::Component`
    /// pointing at a sink that can't itself receive mail.
    pub fn send_reply<K>(&self, sender: ReplyTo, result: &K) -> bool
    where
        K: aether_data::Kind + serde::Serialize,
    {
        match sender.target {
            ReplyTarget::None => false,
            ReplyTarget::Session(_) | ReplyTarget::EngineMailbox { .. } => {
                match self.outbound.get() {
                    Some(outbound) => outbound.send_reply(sender, result),
                    None => false,
                }
            }
            ReplyTarget::Component(mailbox) => {
                let payload = match postcard::to_allocvec(result) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            target: "aether_substrate::mailer",
                            kind = K::NAME,
                            error = %e,
                            "reply encode failed",
                        );
                        return false;
                    }
                };
                // ADR-0042: echo the caller's correlation_id onto the
                // reply envelope so a `wait_reply_p32` parked on this
                // correlation picks the right reply out of the mpsc.
                // Reply target is None — nobody replies to a reply.
                let reply_to = ReplyTo::with_correlation(ReplyTarget::None, sender.correlation_id);
                self.push(Mail::new(mailbox, K::ID, payload, 1).with_reply_to(reply_to));
                true
            }
        }
    }

    /// Block until every live component's inbox is empty and no
    /// `deliver` is in flight. Phase-3 replacement for Phase-2's
    /// `wait_idle` barrier — equivalent end-to-end semantics
    /// (iterates the components table and waits on each entry's
    /// per-mailbox drain counter, re-checking in case one entry's
    /// delivery pushed fresh mail to another).
    pub fn drain_all(&self) {
        let components = self.components.get().expect("Mailer not wired");
        crate::scheduler::drain_all(components);
    }

    /// Budget-aware variant of `drain_all` (ADR-0063). Returns a
    /// `DrainSummary` the chassis matches on to detect dispatcher
    /// deaths and wedges; on either, the chassis routes through
    /// `lifecycle::fatal_abort`.
    pub fn drain_all_with_budget(
        &self,
        budget: std::time::Duration,
    ) -> crate::scheduler::DrainSummary {
        let components = self.components.get().expect("Mailer not wired");
        crate::scheduler::drain_all_with_budget(components, budget)
    }
}

impl Default for Mailer {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `mail.recipient` against the registry + components and
/// dispatch inline. Sinks run on the caller thread; component mail
/// forwards into the per-component dispatcher inbox (which bumps the
/// entry's drain counter). Dropped / unknown recipients and closed
/// inboxes warn-log and drop the mail.
///
/// Mail with a wired `HandleStore` walks through the ADR-0045
/// ref-resolver before recipient dispatch. Schemas with no `Ref`
/// nodes hit the no-op fast path; refs with all handles present
/// splice inline-form bytes into a fresh payload; mail that hits a
/// missing handle parks in the store and returns immediately
/// without dispatch.
fn route_mail(
    mut mail: Mail,
    registry: &Registry,
    components: &ComponentTable,
    outbound: Option<&Arc<HubOutbound>>,
    store: Option<&Arc<HandleStore>>,
) {
    if let Some(store) = store
        && let Some(descriptor) = registry.kind_descriptor(mail.kind)
        && handle_store::schema_contains_ref(&descriptor.schema)
    {
        match handle_store::walk_and_resolve(&descriptor.schema, &mail.payload, store) {
            Ok(WalkOutcome::Resolved { payload }) => {
                if let Cow::Owned(bytes) = payload {
                    mail.payload = bytes;
                }
                // Cow::Borrowed: mail.payload already matches the
                // resolved bytes (no substitutions happened).
            }
            Ok(WalkOutcome::Parked { handle, kind }) => {
                tracing::debug!(
                    target: "aether_substrate::handle_store",
                    handle = %handle,
                    kind = %kind,
                    recipient = ?mail.recipient,
                    "parking mail on missing handle",
                );
                store.park(handle, mail);
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::handle_store",
                    kind = %mail.kind,
                    error = ?e,
                    recipient = ?mail.recipient,
                    "ref-walk failed against registered schema; mail dropped",
                );
                return;
            }
        }
    }

    let recipient = mail.recipient;
    match registry.entry(recipient) {
        Some(MailboxEntry::Sink(handler)) => {
            let kind_name = registry.kind_name(mail.kind).unwrap_or_default();
            // Mail reaching a sink through `push` came from substrate
            // core or a chassis (e.g. the frame loop's FrameStats
            // push, platform input fan-out). Per ADR-0011 origin is
            // `None`. Components reach sinks via `SubstrateCtx::send`
            // inline and never enter `push`.
            handler(
                mail.kind,
                &kind_name,
                None,
                mail.reply_to,
                &mail.payload,
                mail.count,
            );
        }
        Some(MailboxEntry::Component) => {
            let entry = components.read().unwrap().get(&recipient).map(Arc::clone);
            match entry {
                Some(entry) => {
                    // Issue 321 Phase 2: differentiate "actor died
                    // (panic / trap)" from "shutdown closed". The
                    // dead-state check happens before send so the
                    // warn message is unambiguous; otherwise both
                    // failure modes collapsed into the same line and
                    // dead-actor diagnoses became "why is mail being
                    // dropped?" detective work.
                    if entry.is_dead() {
                        tracing::warn!(
                            target: "aether_substrate::queue",
                            mailbox = %recipient,
                            "mail to dead mailbox (actor panicked or trapped); discarded — see component_died broadcast",
                        );
                    } else if !entry.send(mail) {
                        tracing::warn!(
                            target: "aether_substrate::queue",
                            mailbox = %recipient,
                            "component inbox closed (shutdown); mail discarded",
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        target: "aether_substrate::queue",
                        mailbox = %recipient,
                        "mail to registered-component mailbox but no component bound — dropped",
                    );
                }
            }
        }
        Some(MailboxEntry::Dropped) => {
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = %recipient,
                "mail to dropped mailbox — discarded",
            );
        }
        None => {
            // ADR-0037 Phase 1: unknown-locally mailboxes bubble up
            // to the hub-substrate when a live outbound is wired.
            // The hub resolves the id against its own registry and
            // dispatches; if it doesn't know the id either, it
            // warns on its side (end-of-line). Fall back to the
            // local warn-drop when no hub is attached (single-host
            // dev, or the hub chassis itself).
            if let Some(outbound) = outbound
                && outbound.is_connected()
            {
                // ADR-0037 Phase 2: carry the local sending
                // component's mailbox id so the hub can build a
                // `ReplyTo::EngineMailbox { engine_id, mailbox_id }`
                // for the receiving component. `None` for mail
                // with no local component origin (broadcast-
                // originated, substrate-generated).
                let source_mailbox_id = mail.from_component;
                // ADR-0042: carry the correlation through the bubble-
                // up frame so a reply coming back via Phase-2 reply
                // routing lands at the originator's `wait_reply_p32`.
                let correlation_id = mail.reply_to.correlation_id;
                outbound.egress_unresolved_mail(
                    recipient,
                    mail.kind,
                    mail.payload,
                    mail.count,
                    source_mailbox_id,
                    correlation_id,
                );
                return;
            }
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = %recipient,
                "mail to unknown mailbox — dropped",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::handle_store::HandleStore;
    use crate::mail::MailboxId;
    use crate::outbound::EgressEvent;
    use crate::registry::SinkHandler;
    use aether_data::{Kind, Ref};
    use aether_data::{KindDescriptor, NamedField, Primitive, SchemaCell, SchemaType};

    /// ADR-0037 Phase 1: a live outbound + unknown mailbox id
    /// forwards `MailToHubSubstrate` upstream instead of
    /// warn-dropping. The forwarded frame carries the exact
    /// mailbox id / kind / payload / count the caller pushed.
    #[test]
    fn unknown_mailbox_with_connected_outbound_bubbles_up() {
        let (outbound, outbound_rx) = crate::outbound::HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let components = Arc::new(RwLock::new(HashMap::new()));

        let mailer = Mailer::new();
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));
        mailer.wire_outbound(Arc::clone(&outbound));

        let unknown = MailboxId(0xDEADBEEF_u64);
        let kind = KindId(0xABCD_u64);
        let payload = vec![1, 2, 3];
        mailer.push(Mail::new(unknown, kind, payload.clone(), 1));

        let event = outbound_rx.try_recv().expect("bubble-up event emitted");
        match event {
            EgressEvent::UnresolvedMail {
                recipient_mailbox_id,
                kind_id,
                payload: p,
                count,
                ..
            } => {
                assert_eq!(recipient_mailbox_id, unknown);
                assert_eq!(kind_id, kind);
                assert_eq!(p, payload);
                assert_eq!(count, 1);
            }
            other => panic!("expected UnresolvedMail, got {other:?}"),
        }
    }

    /// No outbound wired (or disconnected): unknown mailbox stays
    /// warn-drop. Asserts by showing the outbound channel stays
    /// empty even though we pushed — the warn-drop path doesn't
    /// generate a frame.
    #[test]
    fn unknown_mailbox_without_outbound_warn_drops() {
        let registry = Arc::new(Registry::new());
        let components = Arc::new(RwLock::new(HashMap::new()));

        let mailer = Mailer::new();
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));
        // Deliberately no wire_outbound.

        let unknown = MailboxId(0xDEADBEEF_u64);
        mailer.push(Mail::new(unknown, KindId(0xABCD), vec![], 0));
        // No panic is the test; the warn path logs and returns.
    }

    // ------------------------------------------------------------
    // ADR-0045 Ref-resolution integration
    // ------------------------------------------------------------

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct Note {
        body: String,
        seq: u32,
    }
    impl Kind for Note {
        const NAME: &'static str = "test.mailer_note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: ::aether_data::KindId = ::aether_data::KindId(0xDEAD_BEEF_0003_0001);
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct HeldNote {
        held: Ref<Note>,
        seq: u32,
    }
    impl Kind for HeldNote {
        const NAME: &'static str = "test.mailer_held_note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: ::aether_data::KindId = ::aether_data::KindId(0xDEAD_BEEF_0003_0002);
    }

    fn note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: std::borrow::Cow::Owned(vec![
                NamedField {
                    name: std::borrow::Cow::Borrowed("body"),
                    ty: SchemaType::String,
                },
                NamedField {
                    name: std::borrow::Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    fn held_note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: std::borrow::Cow::Owned(vec![
                NamedField {
                    name: std::borrow::Cow::Borrowed("held"),
                    ty: SchemaType::Ref(SchemaCell::owned(note_schema())),
                },
                NamedField {
                    name: std::borrow::Cow::Borrowed("seq"),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]),
            repr_c: false,
        }
    }

    /// Capture-bytes sink: records every payload it receives so a
    /// test can assert what bytes the dispatcher delivered.
    struct CapturingSink {
        captured: Arc<RwLock<Vec<Vec<u8>>>>,
        delivery_count: Arc<AtomicUsize>,
    }
    impl CapturingSink {
        fn new() -> Self {
            Self {
                captured: Arc::new(RwLock::new(Vec::new())),
                delivery_count: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn handler(&self) -> SinkHandler {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(
                move |_kind_id: KindId,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      _sender: ReplyTo,
                      bytes: &[u8],
                      _count: u32| {
                    captured.write().unwrap().push(bytes.to_vec());
                    count.fetch_add(1, Ordering::SeqCst);
                },
            )
        }
    }

    fn make_mailer() -> (Arc<Registry>, Arc<Mailer>, Arc<HandleStore>) {
        let registry = Arc::new(Registry::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), components);
        mailer.wire_handle_store(Arc::clone(&store));
        (registry, mailer, store)
    }

    /// Mail to a sink whose kind has no `Ref` fields takes the
    /// fast path: no walker invocation, payload delivered verbatim.
    #[test]
    fn ref_free_kind_passes_through_mailer() {
        let (registry, mailer, _store) = make_mailer();
        let note_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
                is_stream: false,
            })
            .unwrap();
        let sink = CapturingSink::new();
        let sink_id = registry.register_sink("test.sink", sink.handler());

        let note = Note {
            body: "verbatim".into(),
            seq: 1,
        };
        let bytes = postcard::to_allocvec(&note).unwrap();
        mailer.push(Mail::new(sink_id, note_id, bytes.clone(), 1));

        let captured = sink.captured.read().unwrap().clone();
        assert_eq!(captured, vec![bytes]);
    }

    /// Mail with a `Handle` ref whose handle is missing parks in the
    /// store. The sink does not see it. After `resolve_handle` lands
    /// the entry, the parked mail re-routes and the sink receives
    /// the spliced payload.
    #[test]
    fn handle_ref_parks_then_resolves_through_mailer() {
        let (registry, mailer, store) = make_mailer();
        let outer_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: HeldNote::NAME.into(),
                schema: held_note_schema(),
                is_stream: false,
            })
            .unwrap();
        let inner_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
                is_stream: false,
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_sink("test.sink", sink.handler());

        // Push HeldNote mail with `held = Handle(7)`. Handle 7 is
        // not in the store yet — the mail must park. We construct
        // `Ref::Handle` directly with the registry-derived
        // `inner_kind_id` so the walker's debug-assert (stored
        // kind_id == wire kind_id) holds when the resolve fires.
        let outer = HeldNote {
            held: Ref::Handle {
                id: 7,
                kind_id: inner_kind_id.0,
            },
            seq: 11,
        };
        let outer_bytes = postcard::to_allocvec(&outer).unwrap();
        mailer.push(Mail::new(sink_id, outer_kind_id, outer_bytes, 1));
        assert_eq!(
            sink.delivery_count.load(Ordering::SeqCst),
            0,
            "mail must not dispatch until handle resolves",
        );
        assert_eq!(store.parked_count(HandleId(7)), 1);

        // Resolve handle 7. The mail should now flow to the sink
        // with the inner Note bytes spliced inline.
        let inner = Note {
            body: "resolved".into(),
            seq: 99,
        };
        let inner_bytes = postcard::to_allocvec(&inner).unwrap();
        mailer
            .resolve_handle(HandleId(7), inner_kind_id, inner_bytes)
            .unwrap();
        assert_eq!(store.parked_count(HandleId(7)), 0);
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 1);

        let captured = sink.captured.read().unwrap();
        let delivered: HeldNote = postcard::from_bytes(&captured[0]).unwrap();
        assert_eq!(delivered.seq, 11);
        match delivered.held {
            Ref::Inline(got) => {
                assert_eq!(got.body, "resolved");
                assert_eq!(got.seq, 99);
            }
            Ref::Handle { .. } => panic!("walker must replace Handle with Inline"),
        }
    }

    /// `resolve_handle` with no `Mailer::wire_handle_store` is a
    /// no-op success — the original pre-PR-2 mailer paths (no store
    /// wired) keep working without panicking.
    #[test]
    fn resolve_handle_without_wired_store_is_noop() {
        let registry = Arc::new(Registry::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        let mailer = Mailer::new();
        mailer.wire(Arc::clone(&registry), components);
        // No wire_handle_store call.
        mailer
            .resolve_handle(HandleId(1), KindId(2), vec![3])
            .unwrap();
    }

    /// Mail whose payload is malformed against the registered
    /// schema (e.g. truncated bytes) gets dropped with a warn log,
    /// not delivered to the sink. Pin the contract — without this
    /// guard the sink would receive bytes that don't decode against
    /// the schema it expects.
    #[test]
    fn malformed_ref_payload_drops_mail() {
        let (registry, mailer, _store) = make_mailer();
        let kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: HeldNote::NAME.into(),
                schema: held_note_schema(),
                is_stream: false,
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_sink("test.sink", sink.handler());

        // Truncated payload — the walker bails Truncated mid-walk.
        mailer.push(Mail::new(sink_id, kind_id, vec![0u8; 1], 1));
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 0);
    }
}
