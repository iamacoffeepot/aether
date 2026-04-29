//! Hub-chassis loopback: the hub as one of its own engines.
//!
//! ADR-0034 Phase 2 sub-phase A. The hub chassis boots its own
//! `SubstrateBoot` (wasmtime + scheduler + mailer + registry +
//! control plane) and registers itself in its own `EngineRegistry`
//! under the reserved `HUB_SELF_ENGINE_ID`. After this, every
//! existing MCP tool (`list_engines`, `describe_kinds`,
//! `load_component`, `send_mail`, `engine_logs`,
//! `describe_component`) works uniformly against the hub — no
//! per-tool special-casing, no new protocol.
//!
//! Two channels bridge the in-process engine with the hub's routing
//! layer:
//!
//!   - **Inbound** (`tokio::mpsc<HubToEngine>`): `mail_tx` goes into
//!     the hub-self `EngineRecord`. MCP tools push frames at the
//!     hub the same way they push at any other engine; the
//!     inbound-drainer task resolves each `Mail` frame against the
//!     substrate's `Registry` and pushes onto the substrate's
//!     `Mailer` via `dispatch_hub_to_engine_mail` — the exact path a
//!     remote engine's `HubClient` reader would take over TCP.
//!
//!   - **Outbound** (`std::sync::mpsc<EngineToHub>`): attached to
//!     `SubstrateBoot.outbound` via `HubOutbound::attach`. Any frame
//!     the substrate emits (component observation `Mail`, control
//!     plane `KindsChanged`, captured `LogBatch`) drains through the
//!     outbound-drainer thread and is dispatched into the hub's
//!     `SessionRegistry` / `EngineRegistry` / `LogStore` the same
//!     way `engine.rs::read_loop` handles frames arriving off a
//!     remote engine's TCP socket.
//!
//! Self-dialling is explicitly avoided: the boot is built and used
//! without ever calling `connect_hub_from_env`, so the substrate
//! does not try to `HubClient::connect` to its own TCP listener
//! (which wouldn't be bound yet at `boot()` time anyway).

use std::collections::HashMap;
use std::sync::Arc;

use aether_hub_protocol::{
    EngineId, EngineMailToHubSubstrateFrame, EngineToHub, HubToEngine, MailByIdFrame, Uuid,
};
use aether_kinds::UnresolvedMail;
use aether_mail::{Kind, mailbox_id_from_name};
use aether_substrate_core::{
    AETHER_DIAGNOSTICS, Mail, MailboxId, Mailer, Registry, ReplyTarget, ReplyTo, SubstrateBoot,
    dispatch_hub_to_engine_mail,
};
use tokio::sync::mpsc;

use crate::log_store::LogStore;
use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::SessionRegistry;

/// Reserved `EngineId` for the hub's own loopback engine. A nil UUID
/// is externally unreachable (engines minted by the TCP handshake
/// always get `Uuid::new_v4()`), and it surfaces uniquely in
/// `list_engines` output so agents can pick it out without a runtime
/// branch.
pub const HUB_SELF_ENGINE_ID: EngineId = EngineId(Uuid::nil());

/// Bound on the hub-self inbound mpsc. Matches the per-engine TCP
/// writer capacity in `engine.rs` so MCP tools see the same
/// back-pressure shape against the hub as against any remote engine.
const INBOUND_CHANNEL_CAPACITY: usize = 256;

/// Everything the hub-chassis needs to drive the in-process engine:
/// the `SubstrateBoot` whose workers and runtime handles must
/// outlive the chassis, plus the two receiver ends of the bridge
/// channels. The chassis spawns two drainer tasks in its tokio
/// runtime that pull from these receivers.
pub struct LoopbackEngine {
    pub boot: SubstrateBoot,
    pub inbound_rx: mpsc::Receiver<HubToEngine>,
    pub outbound_rx: std::sync::mpsc::Receiver<EngineToHub>,
}

/// Cheap clonable handle onto the loopback substrate's registry +
/// mailer, for code paths that dispatch mail into the hub-self
/// engine without going through the `EngineRecord.mail_tx` (ADR-
/// 0037 Phase 1: bubbled-up mail arrives over TCP and skips the
/// name-based `HubToEngine::Mail` channel because senders have
/// only the hashed id on hand). Constructed from a `LoopbackEngine`
/// and passed to the engine listener so its per-connection read
/// loop can push bubbled mail directly onto the loopback's
/// scheduler.
#[derive(Clone)]
pub struct LoopbackHandle {
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
}

impl LoopbackHandle {
    pub fn from_boot(boot: &SubstrateBoot) -> Self {
        Self {
            registry: Arc::clone(&boot.registry),
            queue: Arc::clone(&boot.queue),
        }
    }

    /// Dispatch mail bubbled up from a remote engine (ADR-0037
    /// Phase 1 + Phase 2). The sender has already hashed the target
    /// mailbox's name into `recipient_mailbox_id`; we resolve it
    /// id-based against the loopback registry and push onto the
    /// `Mailer`. Unknown ids emit an `aether.mail.unresolved`
    /// diagnostic back to the originating engine's
    /// `aether.diagnostics` sink (issue #185) so the typo surfaces in
    /// the sender's own `engine_logs`, not only the hub's.
    ///
    /// `source_engine_id` is the originating engine (known by the
    /// hub from the TCP connection, not on the wire). Combined with
    /// `frame.source_mailbox_id` it becomes
    /// `ReplyTo::EngineMailbox { engine_id, mailbox_id }` on the
    /// delivered `Mail` — the hub-resident component's
    /// `ctx.reply(sender)` then routes through
    /// `HubOutbound::send_reply` which forks on the enum variant
    /// and emits `EngineToHub::MailToEngineMailbox` for this case.
    ///
    /// `engines` is the hub's engine registry; needed only on the
    /// unresolved path to look up the originating engine's mail
    /// channel for the diagnostic reach-back. Passed by the caller
    /// (`engine::read_loop`) which already holds it.
    pub fn deliver_bubbled_mail(
        &self,
        source_engine_id: EngineId,
        frame: EngineMailToHubSubstrateFrame,
        engines: &EngineRegistry,
    ) {
        let EngineMailToHubSubstrateFrame {
            recipient_mailbox_id,
            kind_id,
            payload,
            count,
            source_mailbox_id,
            correlation_id,
        } = frame;
        // Kind lookup guards against an engine bubbling up a kind
        // the hub substrate doesn't know — without it the mail
        // would reach a component expecting a different layout.
        if self.registry.kind_name(kind_id).is_none() {
            eprintln!(
                "aether-substrate-hub: bubbled-up mail of unknown kind_id={kind_id} \
                 mailbox_id={recipient_mailbox_id} — dropped"
            );
            return;
        }
        // Recipient lookup: if the hub can't resolve the mailbox id,
        // don't push onto the queue (where it'd warn-drop silently
        // on the hub side). Instead, echo an
        // `aether.mail.unresolved` observation back to the
        // originating engine so the typo surfaces in that engine's
        // own `engine_logs`. ADR-0037 follow-up / issue #185.
        if self
            .registry
            .entry(MailboxId(recipient_mailbox_id))
            .is_none()
        {
            eprintln!(
                "aether-substrate-hub: bubbled-up mail from engine {source_engine_id:?} to \
                 unknown mailbox_id={recipient_mailbox_id:#x} kind_id={kind_id:#x} — returning \
                 `aether.mail.unresolved` diagnostic to originator"
            );
            send_unresolved_diagnostic(engines, source_engine_id, recipient_mailbox_id, kind_id);
            return;
        }
        let sender = match source_mailbox_id {
            Some(mailbox_id) => ReplyTo::with_correlation(
                ReplyTarget::EngineMailbox {
                    engine_id: source_engine_id,
                    mailbox_id,
                },
                correlation_id,
            ),
            None => ReplyTo::with_correlation(ReplyTarget::None, correlation_id),
        };
        self.queue.push(
            Mail::new(MailboxId(recipient_mailbox_id), kind_id, payload, count)
                .with_reply_to(sender),
        );
    }
}

/// Build an `aether.mail.unresolved` `HubToEngine::MailById` frame and
/// push it onto the originating engine's `mail_tx`. Targets the
/// engine's `aether.diagnostics` sink (pre-registered by
/// `SubstrateBoot::build` so resolution is guaranteed on the receiver
/// — no bubble-up loop). Silently drops if the originating engine is
/// no longer connected or its channel is full; this is a best-effort
/// diagnostic, not a guaranteed delivery.
fn send_unresolved_diagnostic(
    engines: &EngineRegistry,
    source_engine_id: EngineId,
    recipient_mailbox_id: u64,
    kind_id: u64,
) {
    let Some(record) = engines.get(&source_engine_id) else {
        return;
    };
    let payload = bytemuck::bytes_of(&UnresolvedMail {
        recipient_mailbox_id: aether_mail::MailboxId(recipient_mailbox_id),
        kind_id: aether_mail::KindId(kind_id),
    })
    .to_vec();
    let frame = HubToEngine::MailById(MailByIdFrame {
        recipient_mailbox_id: mailbox_id_from_name(AETHER_DIAGNOSTICS),
        kind_id: UnresolvedMail::ID,
        payload,
        count: 1,
        correlation_id: 0,
    });
    if let Err(e) = record.mail_tx.try_send(frame) {
        eprintln!(
            "aether-substrate-hub: unresolved-mail diagnostic to engine {source_engine_id:?} \
             dropped: {e}"
        );
    }
}

impl LoopbackEngine {
    /// Boot the in-process substrate, wire the two bridge channels,
    /// and register the hub-self engine record. Must be called
    /// before the hub's TCP + MCP listeners start so the registry
    /// contains the hub-self entry by the time an MCP client
    /// connects.
    pub fn boot(engines: &EngineRegistry) -> wasmtime::Result<Self> {
        // The hub IS the hub — there's no upstream to dial, so we
        // build the substrate but never call `connect_hub_from_env`.
        // The in-process loopback is wired below.
        let boot =
            SubstrateBoot::builder("aether-substrate-hub", env!("CARGO_PKG_VERSION")).build()?;

        let (outbound_tx, outbound_rx) = std::sync::mpsc::channel::<EngineToHub>();
        boot.outbound.attach(outbound_tx);

        let (inbound_tx, inbound_rx) = mpsc::channel::<HubToEngine>(INBOUND_CHANNEL_CAPACITY);

        engines.insert(EngineRecord {
            id: HUB_SELF_ENGINE_ID,
            name: "aether-substrate-hub".to_owned(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            kinds: boot.boot_descriptors.clone(),
            components: HashMap::new(),
            mail_tx: inbound_tx,
            spawned: false,
        });

        Ok(Self {
            boot,
            inbound_rx,
            outbound_rx,
        })
    }
}

/// Drain `inbound_rx` and push each `Mail` frame onto the
/// substrate's scheduler. Runs for the lifetime of the tokio
/// runtime; exits when every `mail_tx` clone is dropped (which in
/// practice means the hub is shutting down).
///
/// Non-`Mail` variants are frames the hub would normally send to a
/// remote engine over TCP. They're harmless to ignore here: the hub
/// will never send `Welcome` twice (we never minted it in the first
/// place), heartbeats aren't meaningful to ourselves, and `Goodbye`
/// is handled by runtime shutdown rather than a frame.
pub async fn run_inbound_drainer(
    mut inbound_rx: mpsc::Receiver<HubToEngine>,
    registry: Arc<aether_substrate_core::Registry>,
    queue: Arc<aether_substrate_core::Mailer>,
) {
    while let Some(frame) = inbound_rx.recv().await {
        match frame {
            HubToEngine::Mail(mail) => dispatch_hub_to_engine_mail(mail, &registry, &queue),
            HubToEngine::MailById(mail) => {
                aether_substrate_core::dispatch_hub_mail_by_id(mail, &registry, &queue)
            }
            HubToEngine::Heartbeat | HubToEngine::Welcome(_) | HubToEngine::Goodbye(_) => {}
        }
    }
}

/// Drain `outbound_rx` and dispatch each frame into the hub's
/// routing layer exactly as `engine.rs::read_loop` would for a
/// remote engine. Runs on a dedicated std::thread because the
/// `std::sync::mpsc::Receiver::recv` call blocks — a tokio task
/// would stall a runtime worker for the thread's lifetime. Exits
/// when the channel closes (every `Sender` clone held inside
/// `HubOutbound` has dropped, which happens at process shutdown).
///
/// `Mail` frames are routed to Claude sessions via the same
/// `crate::engine::route_engine_mail` path remote engines' mail
/// takes (the function is sync — `tokio::sync::mpsc::Sender::try_send`
/// is non-async — so no runtime handle is needed here);
/// `KindsChanged` updates the hub's per-engine descriptor cache;
/// `LogBatch` appends into the shared `LogStore`. `Hello` /
/// `Heartbeat` / `Goodbye` are silently ignored — the loopback
/// substrate never emits them (no handshake to initiate, heartbeat
/// is meaningless against ourselves, shutdown happens via channel
/// close rather than a frame).
pub fn spawn_outbound_drainer(
    outbound_rx: std::sync::mpsc::Receiver<EngineToHub>,
    engines: EngineRegistry,
    sessions: SessionRegistry,
    logs: LogStore,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hub-loopback-outbound".to_owned())
        .spawn(move || {
            while let Ok(frame) = outbound_rx.recv() {
                match frame {
                    EngineToHub::Mail(m) => {
                        crate::engine::route_engine_mail(&sessions, HUB_SELF_ENGINE_ID, m);
                    }
                    EngineToHub::KindsChanged(kinds) => {
                        engines.update_kinds(&HUB_SELF_ENGINE_ID, kinds);
                    }
                    EngineToHub::LogBatch(entries) => {
                        logs.append(HUB_SELF_ENGINE_ID, entries);
                    }
                    EngineToHub::Hello(_) | EngineToHub::Heartbeat | EngineToHub::Goodbye(_) => {}
                    EngineToHub::MailToHubSubstrate(_) => {
                        // The loopback substrate has no upstream
                        // hub (its own boot skips `AETHER_HUB_URL`)
                        // so its `HubOutbound` never sees this
                        // variant. Unreachable under Phase 1
                        // wiring; left as an explicit drop so a
                        // future wiring change can't silently
                        // route hub-self bubbled mail to itself.
                    }
                    EngineToHub::MailToEngineMailbox(frame) => {
                        // ADR-0037 Phase 2: a hub-resident
                        // component replied to a bubbled-up sender.
                        // Look up the target engine's mail_tx in
                        // our registry and forward as `MailById` so
                        // the target engine's hub-client reader
                        // resolves the mailbox id + kind locally
                        // and dispatches. Drops silently if the
                        // originating engine has since disconnected
                        // — the mail was a reply to an engine that
                        // no longer exists.
                        if let Some(record) = engines.get(&frame.target_engine_id) {
                            let by_id = HubToEngine::MailById(MailByIdFrame {
                                recipient_mailbox_id: frame.target_mailbox_id,
                                kind_id: frame.kind_id,
                                payload: frame.payload,
                                count: frame.count,
                                correlation_id: frame.correlation_id,
                            });
                            if let Err(e) = record.mail_tx.try_send(by_id) {
                                eprintln!(
                                    "aether-substrate-hub: reply to engine {:?} dropped: {e}",
                                    frame.target_engine_id,
                                );
                            }
                        } else {
                            eprintln!(
                                "aether-substrate-hub: reply to unknown engine {:?} dropped",
                                frame.target_engine_id,
                            );
                        }
                    }
                }
            }
        })
        .expect("spawn hub-loopback-outbound thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LoopbackEngine::boot` registers the hub in its own engine
    /// registry under the reserved id, so subsequent MCP tool calls
    /// (which look up engines through the same registry) can reach
    /// the hub without any per-tool special-casing. Smoke-checks
    /// the presence, name, and that declared kinds are non-empty
    /// (the boot seeds `aether_kinds::descriptors::all()`).
    #[test]
    fn boot_registers_self_in_engine_registry() {
        let engines = EngineRegistry::new();
        assert!(engines.is_empty());

        let _loopback = LoopbackEngine::boot(&engines).expect("loopback boot");

        let record = engines
            .get(&HUB_SELF_ENGINE_ID)
            .expect("hub-self registered");
        assert_eq!(record.name, "aether-substrate-hub");
        assert_eq!(record.id, HUB_SELF_ENGINE_ID);
        assert!(!record.spawned, "hub-self is not a spawned child");
        assert!(
            !record.kinds.is_empty(),
            "boot descriptors should be non-empty",
        );
    }

    /// ADR-0037 Phase 2: the outbound drainer forwards
    /// `EngineToHub::MailToEngineMailbox` to the target engine's
    /// `mail_tx` as `HubToEngine::MailById`. Proves the reply-path
    /// routing hop without needing a full component stand-up.
    #[tokio::test]
    async fn outbound_drainer_routes_engine_mailbox_reply() {
        use aether_hub_protocol::MailToEngineMailboxFrame;

        use crate::registry::EngineRecord;

        let engines = EngineRegistry::new();
        let sessions = SessionRegistry::new();
        let logs = LogStore::new();

        // Synthesize a target engine with a mail_tx we control.
        let (mail_tx, mut mail_rx) = mpsc::channel::<HubToEngine>(16);
        let target_engine_id = EngineId(Uuid::new_v4());
        engines.insert(EngineRecord {
            id: target_engine_id,
            name: "target".to_owned(),
            pid: 1,
            version: "0".to_owned(),
            kinds: vec![],
            components: HashMap::new(),
            mail_tx,
            spawned: false,
        });

        let (outbound_tx, outbound_rx) = std::sync::mpsc::channel::<EngineToHub>();
        let _thread = spawn_outbound_drainer(outbound_rx, engines, sessions, logs);

        // Simulate a hub-resident component's reply-to-engine-
        // mailbox emission.
        outbound_tx
            .send(EngineToHub::MailToEngineMailbox(MailToEngineMailboxFrame {
                target_engine_id,
                target_mailbox_id: 99,
                kind_id: 42,
                payload: vec![1, 2, 3],
                count: 1,
                correlation_id: 0,
            }))
            .expect("outbound send");

        let got = tokio::time::timeout(std::time::Duration::from_secs(2), mail_rx.recv())
            .await
            .expect("drainer forward timeout")
            .expect("mail_rx closed");
        match got {
            HubToEngine::MailById(frame) => {
                assert_eq!(frame.recipient_mailbox_id, 99);
                assert_eq!(frame.kind_id, 42);
                assert_eq!(frame.payload, vec![1, 2, 3]);
                assert_eq!(frame.count, 1);
            }
            other => panic!("expected MailById, got {other:?}"),
        }
    }

    /// ADR-0037 Phase 1: `deliver_bubbled_mail` on an unknown kind
    /// id must drop silently (no panic, no queue push) — otherwise
    /// a component would receive mail of a layout it doesn't know.
    /// The warn is side-effect only; this test proves the guard
    /// trips by constructing a handle + feeding a synthetic kind
    /// id the registry has never seen.
    #[test]
    fn deliver_bubbled_mail_drops_unknown_kind() {
        let engines = EngineRegistry::new();
        let loopback = LoopbackEngine::boot(&engines).expect("loopback boot");
        let handle = LoopbackHandle::from_boot(&loopback.boot);

        // 0xDEAD_BEEF_DEAD_BEEF is not a valid hashed kind id — the
        // registry has no entry for it, so the kind lookup inside
        // deliver_bubbled_mail returns None and the frame is
        // dropped.
        handle.deliver_bubbled_mail(
            EngineId(Uuid::from_u128(0x1234)),
            EngineMailToHubSubstrateFrame {
                recipient_mailbox_id: 42,
                kind_id: 0xDEAD_BEEF_DEAD_BEEF,
                payload: vec![1, 2, 3],
                count: 1,
                source_mailbox_id: None,
                correlation_id: 0,
            },
            &engines,
        );
        // No panic == pass. The production flow logs a warn and
        // returns; we can't assert on the warn without threading
        // a tracing subscriber, which is out of scope for the
        // Phase-1 smoke.
    }

    /// Issue #185: when the hub can't resolve a bubbled-up mail's
    /// recipient mailbox id, it pushes an
    /// `aether.mail.unresolved` `MailById` frame onto the
    /// originating engine's `mail_tx`. Test by registering a fake
    /// engine record in the registry, feeding a frame whose
    /// recipient id has no hub-side mailbox, and asserting the
    /// registered `mail_tx` receives exactly one `UnresolvedMail`
    /// frame targeting the `aether.diagnostics` sink.
    #[test]
    fn deliver_bubbled_mail_unknown_recipient_emits_diagnostic_to_originator() {
        let engines = EngineRegistry::new();
        let loopback = LoopbackEngine::boot(&engines).expect("loopback boot");
        let handle = LoopbackHandle::from_boot(&loopback.boot);

        let originator_id = EngineId(Uuid::from_u128(0x4242));
        let (mail_tx, mut mail_rx) = mpsc::channel::<HubToEngine>(8);
        engines.insert(EngineRecord {
            id: originator_id,
            name: "test-originator".into(),
            pid: 1,
            version: "0".into(),
            kinds: vec![],
            components: HashMap::new(),
            mail_tx,
            spawned: false,
        });

        // Tick's id is registered; the recipient mailbox id is bogus
        // (no hub-side component binds it, no sink uses it), so the
        // unresolved path fires.
        handle.deliver_bubbled_mail(
            originator_id,
            EngineMailToHubSubstrateFrame {
                recipient_mailbox_id: 0xBAD_C0FFEE_u64,
                kind_id: <aether_kinds::Tick as Kind>::ID,
                payload: vec![],
                count: 1,
                source_mailbox_id: Some(0x5151),
                correlation_id: 0,
            },
            &engines,
        );

        let got = mail_rx
            .try_recv()
            .expect("diagnostic frame pushed onto originator's mail_tx");
        let HubToEngine::MailById(frame) = got else {
            panic!("expected MailById, got {got:?}");
        };
        assert_eq!(frame.kind_id, UnresolvedMail::ID);
        assert_eq!(
            frame.recipient_mailbox_id,
            mailbox_id_from_name(AETHER_DIAGNOSTICS),
        );
        let record: &UnresolvedMail = bytemuck::from_bytes(&frame.payload);
        assert_eq!(
            record.recipient_mailbox_id,
            aether_mail::MailboxId(0xBAD_C0FFEE_u64)
        );
        assert_eq!(
            record.kind_id,
            aether_mail::KindId(<aether_kinds::Tick as Kind>::ID),
        );
        assert_eq!(frame.count, 1);
        assert!(
            mail_rx.try_recv().is_err(),
            "only one diagnostic frame per unresolved recipient",
        );
    }
}
