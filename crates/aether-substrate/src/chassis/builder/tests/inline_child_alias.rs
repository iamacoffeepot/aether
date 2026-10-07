//! Inline-child aliases as first-class addresses: an inline child that is
//! despawned or whose parent closes closes too — its alias tombstones, its
//! watchers are notified, and a despawned alias's route retires.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::spawn::Subname;
use crate::chassis::builder::{Builder, PassiveChassis};
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect};
use crate::mail::registry::{MailboxEntry, Registry, RouteContract};
use crate::testing::canonical_id;
use crate::testing::{TestChassis, await_settled, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{ActorRef, Addressable, ErasedActorRef, HandlesKind};
use aether_data::Kind;
use crossbeam_channel::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

// Tells an `AliasWatcher` which address to monitor.
pod_kind!(AliasWatchOrder { target_id: u64 }, "test.alias_close.watch_order", 0x5AFE_0241_0008_0001);

/// Watcher for the alias-close scenarios: monitors whatever address an
/// `AliasWatchOrder` names and reports the reference it monitored and the
/// sender of each `MonitorNotice` as it handles them. The reference is
/// reported inside the order's own chain, so the scenario reads it after
/// settlement; a notice is a close-tail fan-out outside any root the scenario
/// holds, so the watcher also signals `arrived` once it has reported one.
struct AliasWatcher {
    monitored: Sender<ErasedActorRef>,
    notices: Sender<Option<ErasedActorRef>>,
    arrived: Sender<()>,
    handles: Vec<MonitorHandle>,
}
impl Addressable for AliasWatcher {
    const NAMESPACE: &'static str = "test.alias_close.watcher";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for AliasWatcher {}
impl HandlesKind<AliasWatchOrder> for AliasWatcher {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<aether_kinds::MonitorNotice> for AliasWatcher {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::Lifecycle<Self> for AliasWatcher {
    type Config = ();
    type Params = (Sender<ErasedActorRef>, Sender<Option<ErasedActorRef>>, Sender<()>);
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(
        (): (),
        (monitored, notices, arrived): Self::Params,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<Self, BootError> {
        Ok(Self { monitored, notices, arrived, handles: Vec::new() })
    }
}
impl aether_actor::Declared for AliasWatcher {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for AliasWatcher {
    type State = Self;
}
impl Dispatch<Self> for AliasWatcher {
    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, crate::Anyone, crate::Unchecked>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 == AliasWatchOrder::ID.0 {
            let position = MailboxId(AliasWatchOrder::decode_from_bytes(payload)?.target_id);
            let Ok(target) = ctx.resolve_live(position) else {
                panic!("the watched address must route live at order time");
            };
            state.handles.push(ctx.monitor(target));
            let _ = state.monitored.send(target);
            return Some(());
        }
        if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
            <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
            let _ = state.notices.send(ctx.sender());
            let _ = state.arrived.send(());
            return Some(());
        }
        None
    }
}

/// A live watcher: its reference, and the receivers its registrations and
/// notices report on.
struct WatcherProbe {
    actor: ActorRef<AliasWatcher>,
    monitored: Receiver<ErasedActorRef>,
    notices: Receiver<Option<ErasedActorRef>>,
    arrived: Receiver<()>,
}

impl WatcherProbe {
    fn spawn(chassis: &PassiveChassis<TestChassis>) -> Self {
        let (monitored_tx, monitored) = crossbeam_channel::unbounded();
        let (notices_tx, notices) = crossbeam_channel::unbounded();
        let (arrived_tx, arrived) = crossbeam_channel::unbounded();
        let actor = chassis
            .spawn_actor::<AliasWatcher>(Subname::Counter, (), (monitored_tx, notices_tx, arrived_tx))
            .finish()
            .expect("spawn watcher");
        Self { actor, monitored, notices, arrived }
    }

    /// Order the watcher to monitor `target` and read the reference its
    /// handler reported inside the order's chain.
    fn watch(&self, chassis: &PassiveChassis<TestChassis>, target: MailboxId) -> ErasedActorRef {
        let (_, settled) = chassis.send_tracked(self.actor, &AliasWatchOrder { target_id: target.0 }, None);
        await_settled(&settled, "test.alias_close.watch_order");
        self.monitored.try_recv().expect("the watcher handles its order")
    }

    /// Wait for the next `MonitorNotice` the watcher handles, as its sender.
    fn next_notice(&self) -> Option<ErasedActorRef> {
        await_signal(&self.arrived, "test.alias_close.notice");
        self.notices.try_recv().expect("a departure notice reaches the watcher")
    }
}

/// Publish an inline child's alias route onto the live `host`, exactly as the
/// `spawn_inline_child` host fn stages it: the child's rendered lineage name
/// under the host, folded to its own `MailboxId`.
fn publish_alias(registry: &Registry, host: MailboxId) -> (MailboxId, String) {
    let host_name = registry.mailbox_name(host).expect("host registers a canonical name");
    let alias_name = format!("{host_name}/test.inline.child:widget");
    let alias_id = canonical_id(&alias_name);
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
/// answered with its notice at once, and the alias route retires. Before #4228, `despawn_inline_child`
/// tore the child down guest-side only: the alias kept resolving to the
/// host's slot, so the address outlived the actor it named and no watcher
/// ever heard it depart. Before #7065 the despawn only drained the alias, so a
/// new watcher could still register against the dead child and never hear
/// of it.
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
    impl HandlesKind<DespawnOrder> for Host {
        type Sender = aether_actor::Anyone;
    }
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
        type Parents = ();
    }
    impl NativeActor for Host {
        type State = Self;
    }
    impl Dispatch<Self> for Host {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Anyone, crate::Unchecked>,
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

    let (closed_tx, closed) = crossbeam_channel::unbounded();
    let host = chassis.spawn_actor::<Host>(Subname::Counter, (), closed_tx).finish().expect("spawn host");
    let host_id = host.id();
    let (alias_id, alias_name) = publish_alias(&registry, host_id);

    let watcher = WatcherProbe::spawn(&chassis);
    let alias_ref = watcher.watch(&chassis, alias_id);

    // Order the host to close the watcher's own address first — a live
    // mailbox that is not an alias folded onto this host. It must refuse, or a
    // despawn could tombstone any actor and drain its watchers.
    let orders = [watcher.actor.id(), alias_id]
        .map(|target| chassis.send_tracked(host, &DespawnOrder { target_id: target.0 }, None).1);
    for settled in &orders {
        await_settled(settled, "test.alias_despawn.order");
    }
    let answers: Vec<bool> = (0..2).map(|_| closed.try_recv().expect("the host handles each order")).collect();
    assert_eq!(answers, vec![false, true], "an actor may close an alias folded onto its own mailbox and nothing else");
    assert!(!chassis.actor_registry().is_tombstoned(watcher.actor.id()), "a refused close tombstones nothing");
    assert!(chassis.actor_registry().is_tombstoned(alias_id), "a despawned alias's name is spent (ADR-0241 §8)");

    assert_eq!(
        watcher.next_notice(),
        Some(alias_ref),
        "despawning an inline child must fire a departure notice sent from its alias",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "monitors_of[alias] must drain after fan-out");

    // The alias still routes until the owner-staged retirement lands, so the
    // watcher proves it live. The child it names has closed, so the watch
    // returns and the watcher is posted the alias's notice at once, with no
    // entry left for a close that has already run.
    assert_eq!(
        registry.resolve_route_state(<aether_kinds::MonitorNotice as Kind>::ID, alias_id),
        RouteResolution::Live,
        "the alias still routes until the retirement lands",
    );
    let rewatched = watcher.watch(&chassis, alias_id);
    assert_eq!(
        watcher.next_notice(),
        Some(rewatched),
        "a watch on a despawned alias must be answered with a notice sent from the alias",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "a closed alias holds no watcher");

    // The route itself: retiring it is the owner-staged half the trampoline
    // submits alongside the notice.
    let retired = registry
        .submit(EffectBatch::new(vec![RegistryEffect::RetireAlias(alias_id)]))
        .expect("registry accepts the retire batch");
    assert!(
        retired.wait_timeout(Duration::from_secs(5)).expect("retire batch retires").is_ok(),
        "retiring a live alias must apply",
    );

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
/// that only drains an alias, which would let a later watch register against
/// it and leave its key spawnable after its parent is gone.
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

    let host = chassis.spawn_actor::<Host>(Subname::Counter, (), ()).finish().expect("spawn host");
    let host_id = host.id();
    let (alias_id, _) = publish_alias(&registry, host_id);

    let watcher = WatcherProbe::spawn(&chassis);
    let alias_ref = watcher.watch(&chassis, alias_id);

    let _ = chassis.send_tracked(host, &Quit { tag: 1 }, None);
    chassis.await_closed(host.erase());

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
