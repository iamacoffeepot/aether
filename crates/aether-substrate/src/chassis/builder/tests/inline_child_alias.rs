//! Inline-child aliases as first-class addresses: vacating a host fans a notice
//! out per departing alias, and despawning one inline child retires its alias
//! route and notifies its watchers.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// ADR-0114 §4 × ADR-0079 §8: an inline child is addressed by an alias
/// folded onto its host's mailbox, and its sends stamp that alias — so a
/// cap that keys state on the host-stamped source (`register_route_self`,
/// `subscribe_self`) files the child's rows under the alias, never under
/// the host. Both halves of the identity-keyed contract have to hold on an
/// alias for those rows to be reclaimable: the alias must be monitorable,
/// and vacating the host must fan a `MonitorNotice` out per departing
/// address rather than for the host's own id alone. A watcher here
/// monitors both addresses of one cluster; the host vacates; both notices
/// must land.
#[test]
fn vacate_fires_a_notice_for_each_departing_inline_child_alias() {
    use crate::actor::native::spawn::Subname;
    use crate::actor::registry::MonitorError;
    use crate::mail::registry::MailboxEntry;
    use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect};
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::Mutex;

    // Drives the host's `ctx.vacate()` — the trampoline's `DropComponent`
    // stands in for this in production.
    pod_kind!(VacateOrder { tag: u32 }, "test.alias_vacate.order", 0x5AFE_0114_0079_0001);
    // Tells the watcher which address to monitor.
    pod_kind!(WatchOrder { target_id: u64 }, "test.alias_vacate.watch_order", 0x5AFE_0114_0079_0002);

    // Host — stands in for a wasm trampoline: its mailbox stays live and
    // refillable while its occupant (and the occupant's inline children)
    // goes away.
    struct Host;
    impl Addressable for Host {
        const NAMESPACE: &'static str = "test.alias_vacate.host";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Host {}
    impl HandlesKind<VacateOrder> for Host {}
    impl aether_actor::Lifecycle<Self> for Host {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Host {
        type State = Self;
    }
    impl Dispatch<Self> for Host {
        fn dispatch(
            _state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == VacateOrder::ID.0 {
                let _ = VacateOrder::decode_from_bytes(payload)?;
                ctx.vacate();
                return Some(());
            }
            None
        }
    }

    // Watcher — monitors whatever address a `WatchOrder` names, records
    // each registration's outcome, and records every `MonitorNotice.target`
    // it is handed.
    struct Watcher {
        monitored: Arc<Mutex<Vec<Result<MailboxId, MonitorError>>>>,
        notices: Arc<Mutex<Vec<u64>>>,
        handles: Mutex<Vec<MonitorHandle>>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.alias_vacate.watcher";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Watcher {}
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = ();
        type Params = (Arc<Mutex<Vec<Result<MailboxId, MonitorError>>>>, Arc<Mutex<Vec<u64>>>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { monitored: params.0, notices: params.1, handles: Mutex::new(Vec::new()) })
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let target = MailboxId(WatchOrder::decode_from_bytes(payload)?.target_id);
                state.monitored.lock().unwrap().push(ctx.monitor(target).map(|handle| {
                    state.handles.lock().unwrap().push(handle);
                    target
                }));
                return Some(());
            }
            if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                let notice = <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
                state.notices.lock().unwrap().push(notice.target.0);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let host_id = chassis.spawn_actor::<Host>(Subname::Counter, (), ()).finish().expect("spawn host");

    // Publish the inline child's alias route onto the live host, exactly as
    // the `spawn_inline_child` host fn stages it: the child's rendered
    // lineage name under the host, folded to its own `MailboxId`.
    let host_name = registry.mailbox_name(host_id).expect("host registers a canonical name");
    let alias_name = format!("{host_name}/aether.embedded:widget");
    let alias_id = aether_data::mailbox_id_from_path(&alias_name);
    let published = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id,
            alias_name.clone(),
            host_id,
        ))]))
        .expect("registry accepts the alias batch");
    assert!(
        published.wait_timeout(Duration::from_secs(5)).expect("alias batch retires").is_ok(),
        "the inline child's alias must publish against its live host",
    );
    assert_eq!(registry.lookup(&alias_name), Some(alias_id), "the alias resolves as its own address");

    let monitored = Arc::new(Mutex::new(Vec::new()));
    let notices = Arc::new(Mutex::new(Vec::new()));
    let watcher_id = chassis
        .spawn_actor::<Watcher>(Subname::Counter, (), (Arc::clone(&monitored), Arc::clone(&notices)))
        .finish()
        .expect("spawn watcher");

    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    for target in [host_id, alias_id] {
        watcher_handler.enqueue(registry::test_owned_dispatch(
            <WatchOrder as Kind>::ID,
            &(WatchOrder { target_id: target.0 }).encode_into_bytes(),
            1,
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while monitored.lock().unwrap().len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *monitored.lock().unwrap(),
        vec![Ok(host_id), Ok(alias_id)],
        "the watcher must register against the host and its inline child's alias alike",
    );

    // Vacate the host: its occupant, and with it every inline child the
    // occupant hosted, is gone.
    let MailboxEntry::Inbox { handler: host_handler, .. } = registry.entry(host_id).expect("host sink registered")
    else {
        panic!("expected mailbox entry for host");
    };
    host_handler.enqueue(registry::test_owned_dispatch(
        <VacateOrder as Kind>::ID,
        &(VacateOrder { tag: 1 }).encode_into_bytes(),
        1,
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    while notices.lock().unwrap().len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let mut observed = notices.lock().unwrap().clone();
    observed.sort_unstable();
    let mut expected = vec![host_id.0, alias_id.0];
    expected.sort_unstable();
    assert_eq!(
        observed, expected,
        "a vacate must name every departing address, so state keyed on an inline child's alias is reclaimable",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "monitors_of[alias] must drain after fan-out");
    assert!(chassis.actor_registry().is_live(host_id), "vacate leaves the host mailbox live and refillable");

    drop(chassis);
}

/// Issue 4228: despawning an inline child must reach the same end state a
/// vacate reaches for a departing cluster — the alias's watchers notified and
/// the alias route retired. Before this, `despawn_inline_child` tore the child
/// down guest-side only: the alias kept resolving to the host's slot, so the
/// address outlived the actor it named and no watcher ever heard it depart.
///
/// Drives both halves the guest despawn path composes: `NativeCtx::vacate_alias`
/// for the notice, and the `RetireAlias` effect for the route. The negative
/// case is the ownership guard — an actor must not be able to vacate an address
/// that is not an alias folded onto its own mailbox, or a despawn could drain a
/// peer's watchers.
#[test]
fn despawning_an_inline_child_retires_its_alias_and_notifies_watchers() {
    use crate::actor::native::spawn::Subname;
    use crate::actor::registry::MonitorError;
    use crate::mail::registry::MailboxEntry;
    use crate::mail::registry::RouteResolution;
    use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect};
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::Mutex;

    // Drives the host's `ctx.vacate_alias(target)` — the trampoline's drain of
    // the guest's staged despawns stands in for this in production.
    pod_kind!(DespawnOrder { target_id: u64 }, "test.alias_despawn.order", 0x5AFE_0114_4228_0001);
    // Tells the watcher which address to monitor.
    pod_kind!(WatchOrder { target_id: u64 }, "test.alias_despawn.watch_order", 0x5AFE_0114_4228_0002);

    // Host — stands in for a wasm trampoline hosting an inline child.
    struct Host {
        vacated: Arc<Mutex<Vec<bool>>>,
    }
    impl Addressable for Host {
        const NAMESPACE: &'static str = "test.alias_despawn.host";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Host {}
    impl HandlesKind<DespawnOrder> for Host {}
    impl aether_actor::Lifecycle<Self> for Host {
        type Config = ();
        type Params = Arc<Mutex<Vec<bool>>>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { vacated: params })
        }
    }
    impl NativeActor for Host {
        type State = Self;
    }
    impl Dispatch<Self> for Host {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == DespawnOrder::ID.0 {
                let target = MailboxId(DespawnOrder::decode_from_bytes(payload)?.target_id);
                state.vacated.lock().unwrap().push(ctx.vacate_alias(target));
                return Some(());
            }
            None
        }
    }

    // Watcher — monitors whatever address a `WatchOrder` names and records
    // every `MonitorNotice.target` it is handed.
    struct Watcher {
        monitored: Arc<Mutex<Vec<Result<MailboxId, MonitorError>>>>,
        notices: Arc<Mutex<Vec<u64>>>,
        handles: Mutex<Vec<MonitorHandle>>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.alias_despawn.watcher";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Watcher {}
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = ();
        type Params = (Arc<Mutex<Vec<Result<MailboxId, MonitorError>>>>, Arc<Mutex<Vec<u64>>>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { monitored: params.0, notices: params.1, handles: Mutex::new(Vec::new()) })
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let target = MailboxId(WatchOrder::decode_from_bytes(payload)?.target_id);
                state.monitored.lock().unwrap().push(ctx.monitor(target).map(|handle| {
                    state.handles.lock().unwrap().push(handle);
                    target
                }));
                return Some(());
            }
            if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                let notice = <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
                state.notices.lock().unwrap().push(notice.target.0);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let vacated = Arc::new(Mutex::new(Vec::new()));
    let host_id = chassis.spawn_actor::<Host>(Subname::Counter, (), Arc::clone(&vacated)).finish().expect("spawn host");

    // Publish the inline child's alias route onto the live host, exactly as
    // the `spawn_inline_child` host fn stages it.
    let host_name = registry.mailbox_name(host_id).expect("host registers a canonical name");
    let alias_name = format!("{host_name}/aether.embedded:widget");
    let alias_id = aether_data::mailbox_id_from_path(&alias_name);
    let published = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id, alias_name, host_id,
        ))]))
        .expect("registry accepts the alias batch");
    assert!(
        published.wait_timeout(Duration::from_secs(5)).expect("alias batch retires").is_ok(),
        "the inline child's alias must publish against its live host",
    );

    let monitored = Arc::new(Mutex::new(Vec::new()));
    let notices = Arc::new(Mutex::new(Vec::new()));
    let watcher_id = chassis
        .spawn_actor::<Watcher>(Subname::Counter, (), (Arc::clone(&monitored), Arc::clone(&notices)))
        .finish()
        .expect("spawn watcher");

    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    watcher_handler.enqueue(registry::test_owned_dispatch(
        <WatchOrder as Kind>::ID,
        &(WatchOrder { target_id: alias_id.0 }).encode_into_bytes(),
        1,
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    while monitored.lock().unwrap().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *monitored.lock().unwrap(),
        vec![Ok(alias_id)],
        "the watcher must be able to register against the inline child's alias",
    );

    // Order the host to vacate the watcher's own address first — a live
    // mailbox that is not an alias folded onto this host. It must refuse, or a
    // despawn could drain any actor's watchers.
    let MailboxEntry::Inbox { handler: host_handler, .. } = registry.entry(host_id).expect("host sink registered")
    else {
        panic!("expected mailbox entry for host");
    };
    for target in [watcher_id, alias_id] {
        host_handler.enqueue(registry::test_owned_dispatch(
            <DespawnOrder as Kind>::ID,
            &(DespawnOrder { target_id: target.0 }).encode_into_bytes(),
            1,
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while vacated.lock().unwrap().len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *vacated.lock().unwrap(),
        vec![false, true],
        "an actor may vacate an alias folded onto its own mailbox and nothing else",
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while notices.lock().unwrap().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *notices.lock().unwrap(),
        vec![alias_id.0],
        "despawning an inline child must fire a departure notice naming its alias",
    );
    assert_eq!(chassis.actor_registry().monitor_count(alias_id), 0, "monitors_of[alias] must drain after fan-out");

    // The route itself: retiring it is the owner-staged half the trampoline
    // submits alongside the notice.
    assert!(registry.is_live_alias(alias_id), "the alias still routes until the retirement lands");
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
    assert!(chassis.actor_registry().is_live(host_id), "retiring one alias leaves its host live and addressable");

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
