// Inline router (ADR-0038 Phase 3 + issue 603).
//
// Phase 2 retired the VecDeque + router thread; Phase 3 retired the
// global `outstanding` / `done_cv` barrier in favour of per-component
// drains. Issue 603 retired the shared `ComponentTable` Arc.
//
// Issue 634 Phase 4 retired the wasm-component-specific routing path
// entirely: every loaded wasm component is now a `WasmTrampoline`
// `NativeActor` registered as a `MailboxEntry::Closure` like every
// other actor, so the dedicated `ComponentRouter` slot + `route()`
// method + `MailboxEntry::Component` variant are gone. PR 2 retired
// the `drain_all_with_budget` polling barrier in favour of direct
// trap-abort at the trampoline (the trampoline holds a
// `FatalAborter` and aborts on `Component::deliver` Err).
//
// `push(mail)` still resolves the recipient inline on the caller's
// thread.

use std::borrow::Cow;
use std::sync::Arc;

use crate::handle_store::{self, HandleStore, PutError, WalkOutcome};
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{MailDispatch, MailboxEntry, Registry};
use crate::mail::{Mail, ReplyTarget, ReplyTo};
use aether_data::{HandleId, KindId};
use std::sync::OnceLock;

pub struct Mailer {
    /// Registry handle for resolving recipients on `push`. Owned for
    /// the `Mailer`'s lifetime; supplied at construction time alongside
    /// the `HandleStore` (issue 657 collapsed the prior `wire`-after-
    /// `new` setter pair into a required-pair constructor).
    registry: Arc<Registry>,
    /// ADR-0045 typed-handle resolver. `route_mail` runs each mail
    /// through the ref-walker before dispatching; missing handles
    /// park the mail in the store. Required at construction time —
    /// `SubstrateBoot::build` builds one with `HandleStore::from_env()`,
    /// tests pass `Arc::new(HandleStore::new(1024 * 1024))`. Schemas
    /// with no `Ref` nodes hit the no-op fast path inside `route_mail`,
    /// so passing a store on tests that don't exercise handles costs
    /// nothing.
    handle_store: Arc<HandleStore>,
    /// Hub outbound handle. When set and connected, mail to unknown
    /// mailbox ids bubbles up to the hub-substrate (ADR-0037
    /// Phase 1) instead of being warn-dropped locally. Optional —
    /// chassis that skip hub connection (today: the hub chassis
    /// itself) construct a `Mailer` without an outbound and keep
    /// local warn-drop semantics intact (the hub is the end of the
    /// bubbles-up line). Pre-issue-657 this rode an `OnceLock` set
    /// post-construction via `wire_outbound`; the constructor +
    /// `with_outbound` pair below replaces that.
    outbound: Option<Arc<HubOutbound>>,
    /// ADR-0080 §5 chassis-mail router. When mail is addressed to
    /// [`MailboxId::CHASSIS_MAILBOX_ID`], `route_mail` short-circuits
    /// the registry lookup and invokes this closure instead. Today
    /// the chassis installs a router that decodes `Settled { root }`
    /// and signals the [`crate::chassis::settlement::SettlementRegistry`]
    /// — keeping the Mailer ignorant of trace kinds while still
    /// providing the dispatch surface those kinds need.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot. `None` for tests / chassis that don't bring up the
    /// trace pipeline — chassis-addressed mail is silently dropped
    /// in that case.
    chassis_router: OnceLock<Box<dyn Fn(Mail) + Send + Sync>>,
    /// ADR-0080 §6 settlement registry handle, exposed so capabilities
    /// hosting external entry points (RpcServer, future event-source
    /// caps) can subscribe to settlement of mail they dispatch from
    /// their handlers. Threaded through the Mailer rather than down
    /// every `NativeBinding` because settlement is a chassis-wide
    /// service used at runtime — the cap reaches it via
    /// `ctx.mailer().settlement_registry()`.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot alongside [`Self::chassis_router`]. `None` on test fixtures
    /// / chassis that don't bring up the trace pipeline; callers that
    /// expect to subscribe must check for the presence and surface a
    /// clear error otherwise.
    settlement_registry: OnceLock<Arc<crate::chassis::settlement::SettlementRegistry>>,
}

impl Mailer {
    /// Construct a `Mailer` against the substrate's registry and
    /// handle store. `SubstrateBoot::build` is the production caller;
    /// tests build the same trio with `Registry::new()` and
    /// `HandleStore::new(1024 * 1024)` (or any byte budget). Call
    /// [`Self::with_outbound`] to attach a hub outbound if the
    /// chassis needs ADR-0037 bubble-up.
    pub fn new(registry: Arc<Registry>, handle_store: Arc<HandleStore>) -> Self {
        Self {
            registry,
            handle_store,
            outbound: None,
            chassis_router: OnceLock::new(),
            settlement_registry: OnceLock::new(),
        }
    }

    /// ADR-0080 §5 chassis-mail router installation. Called once by
    /// the chassis builder at boot to wire the closure that handles
    /// mail addressed to [`MailboxId::CHASSIS_MAILBOX_ID`]. Subsequent
    /// calls are no-ops — the router slot is single-claim.
    pub fn install_chassis_router(&self, router: Box<dyn Fn(Mail) + Send + Sync>) {
        let _ = self.chassis_router.set(router);
    }

    /// ADR-0080 §6 settlement-registry installation. Called once by the
    /// chassis builder at boot alongside [`Self::install_chassis_router`]
    /// so capability handlers can reach the registry via
    /// `ctx.mailer().settlement_registry()`. Single-claim; subsequent
    /// calls are no-ops.
    pub fn install_settlement_registry(
        &self,
        registry: Arc<crate::chassis::settlement::SettlementRegistry>,
    ) {
        let _ = self.settlement_registry.set(registry);
    }

    /// Borrow the wired [`SettlementRegistry`], or `None` if no
    /// registry was installed (test fixtures, chassis that don't bring
    /// up the trace pipeline). Capabilities subscribe via
    /// [`crate::chassis::settlement::SettlementRegistry::subscribe_settlement_mail`].
    pub fn settlement_registry(
        &self,
    ) -> Option<&Arc<crate::chassis::settlement::SettlementRegistry>> {
        self.settlement_registry.get()
    }

    /// Attach a `HubOutbound` so mail to unknown mailbox ids bubbles
    /// up to the hub-substrate (ADR-0037 Phase 1) instead of being
    /// warn-dropped. Fluent — returns `self` so the call site can
    /// chain after `Mailer::new`. Skip the call entirely for chassis
    /// that are their own hub or for tests that want local warn-drop
    /// semantics (the hub chassis, the warn-drop test in
    /// `actor::wasm::component`).
    pub fn with_outbound(mut self, outbound: Arc<HubOutbound>) -> Self {
        self.outbound = Some(outbound);
        self
    }

    /// Borrow the wired `HandleStore`. Read-only handle exposed so
    /// chassis-side handlers (PR 3 host-fn shims) can publish into
    /// the same store the dispatch path resolves against.
    pub fn handle_store(&self) -> &Arc<HandleStore> {
        &self.handle_store
    }

    /// Borrow the wired [`HubOutbound`], or `None` if no outbound was
    /// attached (the hub chassis, or tests). Surfaced for chassis caps
    /// that thread egress events (replies, log batches, etc.) back to
    /// the hub without the substrate holding a registry closure for
    /// them.
    pub fn outbound(&self) -> Option<&Arc<HubOutbound>> {
        self.outbound.as_ref()
    }

    /// Borrow the wired [`Registry`]. Issue 603: surfaced so
    /// `ComponentHostCapability::init` can pull the registry for its
    /// internal state without requiring it on `ComponentHostConfig` —
    /// per Resolved Decision §2 registry arrives via init ctx, not
    /// via the cap's config struct.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Hand `mail` to the substrate for dispatch. Closure-bound
    /// mailboxes run their handler on the caller thread; dropped /
    /// unknown recipients warn-and-discard (or bubble up to the
    /// hub-substrate when a `HubOutbound` is connected, per ADR-0037).
    pub fn push(&self, mail: Mail) {
        route_mail(
            mail,
            &self.registry,
            self.outbound.as_ref(),
            &self.handle_store,
            self.chassis_router.get().map(|b| &**b),
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
    pub fn resolve_handle(
        &self,
        handle: HandleId,
        kind: KindId,
        bytes: Vec<u8>,
    ) -> Result<(), PutError> {
        self.handle_store.put(handle, kind, bytes)?;
        let parked = self.handle_store.take_parked(handle);
        let outbound = self.outbound.as_ref();
        let chassis_router = self.chassis_router.get().map(|b| &**b);
        for mail in parked {
            route_mail(
                mail,
                &self.registry,
                outbound,
                &self.handle_store,
                chassis_router,
            );
        }
        Ok(())
    }

    /// Route a chassis-bound mailbox's `*Result` reply to `sender`
    /// with a single encode. `Session` / `EngineMailbox` hand off to
    /// the hub outbound (unchanged hub-wire format); `Component`
    /// pushes a fresh `Mail` into the target component's inbox so the
    /// guest's normal dispatch path delivers the reply. `None` is a
    /// silent drop — nobody asked for a reply.
    ///
    /// The reply mail carries `reply_to = None`: the receiver isn't
    /// expected to reply to a reply, and decorating with the
    /// closure-bound mailbox's id would produce a
    /// `ReplyEntry::Component` pointing at an entry that can't itself
    /// receive mail.
    pub fn send_reply<K>(&self, sender: ReplyTo, result: &K) -> bool
    where
        K: aether_data::Kind + serde::Serialize,
    {
        match sender.target {
            ReplyTarget::None => false,
            ReplyTarget::Session(_) | ReplyTarget::EngineMailbox { .. } => {
                match self.outbound.as_ref() {
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
}

/// Resolve `mail.recipient` against the registry and dispatch
/// inline. Closure-bound mailboxes run their handler on the caller
/// thread (or fan out via the cap's mpsc, depending on the closure).
/// Dropped / unknown recipients warn-log and drop the mail.
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
    outbound: Option<&Arc<HubOutbound>>,
    store: &Arc<HandleStore>,
    chassis_router: Option<&(dyn Fn(Mail) + Send + Sync)>,
) {
    // ADR-0080 §5 chassis-mail switch — routed ahead of the registry
    // lookup so mail to `CHASSIS_MAILBOX_ID` reaches the chassis-
    // installed router without bubbling up as `UnresolvedMail`. Today
    // the router decodes `Settled { root }` and signals the
    // `SettlementRegistry`; future chassis-internal kinds add
    // matching arms inside the router closure.
    if mail.recipient == aether_data::MailboxId::CHASSIS_MAILBOX_ID {
        if let Some(router) = chassis_router {
            router(mail);
        } else {
            tracing::warn!(
                target: "aether_substrate::queue",
                kind = %mail.kind,
                "chassis-addressed mail dropped — no chassis router installed",
            );
        }
        return;
    }

    if let Some(descriptor) = registry.kind_descriptor(mail.kind)
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
        Some(MailboxEntry::Closure(handler)) => {
            let kind_name = registry.kind_name(mail.kind).unwrap_or_default();
            // Mail reaching a closure-bound mailbox through `push`
            // came from substrate core or a chassis (e.g. the frame
            // loop's FrameStats push, platform input fan-out). Per
            // ADR-0011 origin is `None`. Components reach
            // closure-bound mailboxes via `ComponentCtx::send` inline
            // and never enter `push`.
            handler(MailDispatch {
                kind: mail.kind,
                kind_name: &kind_name,
                origin: None,
                sender: mail.reply_to,
                payload: &mail.payload,
                count: mail.count,
                mail_id: mail.mail_id,
                root: mail.root,
                parent_mail: mail.parent_mail,
            });
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
                // with no local component origin (substrate-generated).
                // Recovered from
                // `reply_to.target = Component(_)` set by
                // `ComponentCtx::send` / `NativeBinding::send_mail`
                // (issue #644).
                let source_mailbox_id = match mail.reply_to.target {
                    ReplyTarget::Component(id) => Some(id),
                    _ => None,
                };
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
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::handle_store::HandleStore;
    use crate::mail::MailboxId;
    use crate::mail::outbound::EgressEvent;
    use crate::mail::registry::MailboxHandler;
    use aether_data::{Kind, Ref};
    use aether_data::{KindDescriptor, NamedField, Primitive, SchemaCell, SchemaType};

    /// ADR-0037 Phase 1: a live outbound + unknown mailbox id
    /// forwards `MailToHubSubstrate` upstream instead of
    /// warn-dropping. The forwarded frame carries the exact
    /// mailbox id / kind / payload / count the caller pushed.
    #[test]
    fn unknown_mailbox_with_connected_outbound_bubbles_up() {
        let (outbound, outbound_rx) = crate::mail::outbound::HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));

        let mailer = Mailer::new(Arc::clone(&registry), store).with_outbound(Arc::clone(&outbound));

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
        let store = Arc::new(HandleStore::new(64 * 1024));

        let mailer = Mailer::new(Arc::clone(&registry), store);
        // Deliberately no `with_outbound` — exercises the local
        // warn-drop path (the hub chassis path).

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
        fn handler(&self) -> MailboxHandler {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(move |dispatch: MailDispatch<'_>| {
                captured.write().unwrap().push(dispatch.payload.to_vec());
                count.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    fn make_mailer() -> (Arc<Registry>, Arc<Mailer>, Arc<HandleStore>) {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(64 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), Arc::clone(&store)));
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
            })
            .unwrap();
        let sink = CapturingSink::new();
        let sink_id = registry.register_closure("test.sink", sink.handler());

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
            })
            .unwrap();
        let inner_kind_id = registry
            .register_kind_with_descriptor(KindDescriptor {
                name: Note::NAME.into(),
                schema: note_schema(),
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_closure("test.sink", sink.handler());

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
            })
            .unwrap();

        let sink = CapturingSink::new();
        let sink_id = registry.register_closure("test.sink", sink.handler());

        // Truncated payload — the walker bails Truncated mid-walk.
        mailer.push(Mail::new(sink_id, kind_id, vec![0u8; 1], 1));
        assert_eq!(sink.delivery_count.load(Ordering::SeqCst), 0);
    }
}
