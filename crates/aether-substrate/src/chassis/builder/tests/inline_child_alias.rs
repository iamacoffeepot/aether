//! Inline-child aliases as first-class addresses: an inline child that is
//! despawned or whose parent closes closes too — its alias tombstones, its
//! watchers are notified, and a despawned alias's route retires.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::spawn::Subname;
use crate::actor::registry::MonitorError;
use crate::chassis::builder::{Builder, PassiveChassis};
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect};
use crate::mail::registry::lineage_mailbox_id;
use crate::mail::registry::{MailboxEntry, Registry, RouteContract};
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, ErasedActorRef, HandlesKind};
use aether_data::Kind;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

// Tells an `AliasWatcher` which address to monitor.
pod_kind!(AliasWatchOrder { target_id: u64 }, "test.alias_close.watch_order", 0x5AFE_0241_0008_0001);

/// Watcher for the alias-close scenarios: monitors whatever address an
/// `AliasWatchOrder` names and reports each registration's outcome and the
/// sender of each `MonitorNotice` as it handles them, so a scenario blocks on
/// the handler having run rather than polling shared state.
struct AliasWatcher {
    monitored: Sender<Result<ErasedActorRef, MonitorError>>,
    notices: Sender<Option<ErasedActorRef>>,
    handles: Vec<MonitorHandle>,
}
impl Addressable for AliasWatcher {
    const NAMESPACE: &'static str = "test.alias_close.watcher";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for AliasWatcher {}
impl HandlesKind<AliasWatchOrder> for AliasWatcher {}
impl HandlesKind<aether_kinds::MonitorNotice> for AliasWatcher {}
impl aether_actor::Lifecycle<Self> for AliasWatcher {
    type Config = ();
    type Params = (Sender<Result<ErasedActorRef, MonitorError>>, Sender<Option<ErasedActorRef>>);
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), (monitored, notices): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { monitored, notices, handles: Vec::new() })
    }
}
impl aether_actor::Declared for AliasWatcher {
    type Depends = ();
    type Spawns = ();
}
impl NativeActor for AliasWatcher {
    type State = Self;
}
impl Dispatch<Self> for AliasWatcher {
    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, crate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 == AliasWatchOrder::ID.0 {
            let position = MailboxId(AliasWatchOrder::decode_from_bytes(payload)?.target_id);
            let outcome = ctx.resolve_live(position).map_err(|_| MonitorError::TargetNotFound).and_then(|target| {
                ctx.monitor(target).map(|handle| {
                    state.handles.push(handle);
                    target
                })
            });
            let _ = state.monitored.send(outcome);
            return Some(());
        }
        if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
            <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
            let _ = state.notices.send(ctx.sender());
            return Some(());
        }
        None
    }
}

/// A live watcher: its id, and the receivers its registrations and notices
/// report on.
struct WatcherProbe {
    id: MailboxId,
    monitored: Receiver<Result<ErasedActorRef, MonitorError>>,
    notices: Receiver<Option<ErasedActorRef>>,
}

impl WatcherProbe {
    fn spawn(chassis: &PassiveChassis<TestChassis>) -> Self {
        let (monitored_tx, monitored) = mpsc::channel();
        let (notices_tx, notices) = mpsc::channel();
        let id = chassis
            .spawn_actor::<AliasWatcher>(Subname::Counter, (), (monitored_tx, notices_tx))
            .finish_commit()
            .expect("spawn watcher");
        Self { id, monitored, notices }
    }

    /// Order the watcher to monitor `target` and wait for its handler to
    /// report the outcome.
    fn watch(&self, registry: &Registry, target: MailboxId) -> Result<ErasedActorRef, MonitorError> {
        enqueue(
            registry,
            self.id,
            <AliasWatchOrder as Kind>::ID,
            &(AliasWatchOrder { target_id: target.0 }).encode_into_bytes(),
        );
        self.monitored.recv_timeout(Duration::from_secs(5)).expect("the watcher handles its order")
    }

    /// Wait for the next `MonitorNotice` the watcher handles, as its sender.
    fn next_notice(&self) -> Option<ErasedActorRef> {
        self.notices.recv_timeout(Duration::from_secs(5)).expect("a departure notice reaches the watcher")
    }
}

/// Push one mail into `target`'s inbox through its registered sink.
fn enqueue(registry: &Registry, target: MailboxId, kind: KindId, payload: &[u8]) {
    let MailboxEntry::Inbox { handler, .. } = registry.entry_at(target).expect("target sink registered") else {
        panic!("expected mailbox entry for {target}");
    };
    handler.enqueue(registry::test_owned_dispatch(kind, payload, 1));
}

/// Publish an inline child's alias route onto the live `host`, exactly as the
/// `spawn_inline_child` host fn stages it: the child's rendered lineage name
/// under the host, folded to its own `MailboxId`.
fn publish_alias(registry: &Registry, host: MailboxId) -> (MailboxId, String) {
    let host_name = registry.mailbox_name(host).expect("host registers a canonical name");
    let alias_name = format!("{host_name}/test.inline.child:widget");
    let alias_id = lineage_mailbox_id(&alias_name);
    let published = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id,
            alias_name.clone(),
            host,
            RouteContract::empty(),
        ))]))
        .expect("registry accepts the alias batch");
    assert!(
        published.wait_timeout(Duration::from_secs(5)).expect("alias batch retires").is_ok(),
        "the inline child's alias must publish against its live host",
    );
    (alias_id, alias_name)
}

/// Issue 4228 and ADR-0241 §8: despawning an inline child closes it. Its
/// alias's watchers are notified, the alias tombstones so a later watch is
/// refused, and the alias route retires. Before #4228, `despawn_inline_child`
/// tore the child down guest-side only: the alias kept resolving to the
/// host's slot, so the address outlived the actor it named and no watcher
/// ever heard it depart. Before #7065 the despawn only drained the alias, so a
/// new watcher could still register against the dead child.
///
/// Drives both halves the guest despawn path composes: `NativeCtx::close_alias`
/// for the tombstone and the notice, and the `RetireAlias` effect for the
/// route. The negative case is the ownership guard — an actor must not be able
/// to close an address that is not an alias folded onto its own mailbox, or a
/// despawn could tombstone a peer and drain its watchers.
#[test]
fn despawning_an_inline_child_retires_its_alias_and_notifies_watchers() {
    use crate::mail::registry::PreparedAliasRetirement;
    use crate::mail::registry::RouteResolution;

    // Drives the host's `ctx.close_alias(&retirement)` — the trampoline's drain
    // of the guest's staged despawns stands in for this in production.
    pod_kind!(DespawnOrder { target_id: u64 }, "test.alias_despawn.order", 0x5AFE_0114_4228_0001);

    // Host — stands in for a wasm trampoline hosting an inline child, and
    // reports each `close_alias` answer as it handles the order.
    struct Host {
        closed: Sender<bool>,
    }
    impl Addressable for Host {
        const NAMESPACE: &'static str = "test.alias_despawn.host";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Host {}
    impl HandlesKind<DespawnOrder> for Host {}
    impl aether_actor::Lifecycle<Self> for Host {
        type Config = ();
        type Params = Sender<bool>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), closed: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { closed })
        }
    }
    impl aether_actor::Declared for Host {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for Host {
        type State = Self;
    }
    impl Dispatch<Self> for Host {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == DespawnOrder::ID.0 {
                let target = MailboxId(DespawnOrder::decode_from_bytes(payload)?.target_id);
                let retirement = PreparedAliasRetirement::new(target, "test.alias_despawn.target");
                let _ = state.closed.send(ctx.close_alias(&retirement));
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let (closed_tx, closed) = mpsc::channel();
    let host_id = chassis.spawn_actor::<Host>(Subname::Counter, (), closed_tx).finish_commit().expect("spawn host");
    let (alias_id, alias_name) = publish_alias(&registry, host_id);

    let watcher = WatcherProbe::spawn(&chassis);
    let alias_ref = watcher
        .watch(&registry, alias_id)
        .expect("the watcher must be able to register against the inline child's alias");

    // Order the host to close the watcher's own address first — a live
    // mailbox that is not an alias folded onto this host. It must refuse, or a
    // despawn could tombstone any actor and drain its watchers.
    for target in [watcher.id, alias_id] {
        enqueue(
            &registry,
            host_id,
            <DespawnOrder as Kind>::ID,
            &(DespawnOrder { target_id: target.0 }).encode_into_bytes(),
        );
    }
    let answers: Vec<bool> =
        (0..2).map(|_| closed.recv_timeout(Duration::from_secs(5)).expect("the host handles each order")).collect();
    assert_eq!(answers, vec![false, true], "an actor may close an alias folded onto its own mailbox and nothing else");
    assert!(!chassis.actor_registry().is_tombstoned(watcher.id), "a refused close tombstones nothing");
    assert!(chassis.actor_registry().is_tombstoned(alias_id), "a despawned alias's name is spent (ADR-0241 §8)");

    assert_eq!(
        watcher.next_notice(),
        Some(alias_ref),
        "despawning an inline child must fire a departure notice sent from its alias",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "monitors_of[alias] must drain after fan-out");

    // The alias still routes until the owner-staged retirement lands, so the
    // watcher proves it live; the monitor is refused all the same, because
    // the child it named has closed.
    assert!(registry.is_live_alias(alias_id), "the alias still routes until the retirement lands");
    assert_eq!(
        watcher.watch(&registry, alias_id),
        Err(MonitorError::TargetTombstoned),
        "a despawned alias must refuse a new watcher rather than hold it past the child's close",
    );

    // The route itself: retiring it is the owner-staged half the trampoline
    // submits alongside the notice.
    let retired = registry
        .submit(EffectBatch::new(vec![RegistryEffect::RetireAlias(alias_id)]))
        .expect("registry accepts the retire batch");
    assert!(
        retired.wait_timeout(Duration::from_secs(5)).expect("retire batch retires").is_ok(),
        "retiring a live alias must apply",
    );

    assert!(!registry.is_live_alias(alias_id), "a despawned alias must stop resolving to its host's slot");
    assert_eq!(
        registry.resolve_route_state(<aether_kinds::MonitorNotice as Kind>::ID, alias_id),
        RouteResolution::Dropped,
        "mail to a despawned alias must report the address as retired, not as never-registered",
    );
    assert!(chassis.actor_registry().is_live_at(host_id), "retiring one alias leaves its host live and addressable");

    // The retired alias name is spent (ADR-0079 §7): re-publishing it under
    // the same parent is refused rather than reviving the address.
    let republished = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id,
            alias_name,
            host_id,
            RouteContract::empty(),
        ))]))
        .expect("registry accepts the re-publish batch");
    assert!(
        republished.wait_timeout(Duration::from_secs(5)).expect("re-publish batch retires").is_err(),
        "a retired alias name must never publish again",
    );
    assert!(matches!(registry.entry_at(alias_id), Some(MailboxEntry::Dropped)), "the retired alias stays Dropped");

    // Idempotent, so a re-despawn of an already-gone alias is a clean no-op —
    // the guest contract `despawn_inline_child` promises.
    let again = registry
        .submit(EffectBatch::new(vec![RegistryEffect::RetireAlias(alias_id)]))
        .expect("registry accepts the repeat retire batch");
    assert!(
        again.wait_timeout(Duration::from_secs(5)).expect("repeat retire batch retires").is_ok(),
        "retiring an already-retired alias must be a clean no-op, not an error",
    );

    drop(chassis);
}

/// ADR-0241 §8: an inline child ends by closing, and so does every inline
/// child of a parent that closes. The close tail tombstones each alias folded
/// onto the closing actor before fanning its notice out, so a watcher that
/// hears the child depart finds its name already spent. Catches a close tail
/// that only drains an alias, which would leave it watchable and its key
/// spawnable after its parent is gone.
#[test]
fn a_closing_parent_tombstones_its_inline_children() {
    // Self-shutdown trigger for the host.
    pod_kind!(Quit { tag: u32 }, "test.alias_close.quit", 0x5AFE_0241_0008_0002);

    // Host — stands in for a wasm trampoline hosting an inline child; it
    // closes on `Quit`.
    unit_shutdown_actor!(Host, "test.alias_close.host", Quit);

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let host_id = chassis.spawn_actor::<Host>(Subname::Counter, (), ()).finish_commit().expect("spawn host");
    let (alias_id, _) = publish_alias(&registry, host_id);

    let watcher = WatcherProbe::spawn(&chassis);
    let alias_ref = watcher
        .watch(&registry, alias_id)
        .expect("the watcher must be able to register against the inline child's alias");

    enqueue(&registry, host_id, <Quit as Kind>::ID, &(Quit { tag: 1 }).encode_into_bytes());

    assert_eq!(
        watcher.next_notice(),
        Some(alias_ref),
        "a closing parent must fire a departure notice from each inline child's alias",
    );
    assert!(chassis.actor_registry().is_tombstoned(host_id), "the closed host's name is spent");
    assert!(
        chassis.actor_registry().is_tombstoned(alias_id),
        "a closing parent's inline child closes with it, so its alias's name is spent too",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "monitors_of[alias] must drain after fan-out");

    drop(chassis);
}
