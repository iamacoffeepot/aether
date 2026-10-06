//! The `watch_p32` / `unwatch_p32` / `watch_ended_p32` host fns
//! (ADR-0079 §8): a WAT guest whose exports forward straight to the imports,
//! over a registry holding a watcher route and a target route, so each test
//! reads the host's answers and the registry's monitor indices with no SDK
//! between.

use std::sync::Arc;

use wasmtime::{Engine, Linker, Module, Store, TypedFunc};

use super::{WAT_REALLOC, ctx_at};
use crate::actor::wasm::component::ComponentCtx;
use crate::actor::wasm::host_fns;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
use crate::testing::{bare_substrate, registered_ref};

/// Two watched-type tags, as the SDK would fold them from two types.
const PROVIDER: u64 = 0x1001;
const AUDITOR: u64 = 0x1002;

/// A registry with a watcher route and a target route, and their positions.
struct Routes {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    watcher: MailboxId,
    target: MailboxId,
}

impl Routes {
    fn new() -> Self {
        let (registry, mailer) = bare_substrate();
        let discharging = || -> Arc<dyn InboxHandler> { Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()) };
        let watcher = registered_ref(&registry, "test.wasm.watch.watcher", discharging()).id();
        let target = registered_ref(&registry, "test.wasm.watch.target", discharging()).id();

        Self { registry, mailer, watcher, target }
    }

    /// How many watchers the registry holds for the target.
    fn watchers_of_target(&self) -> usize {
        self.registry.actor_registry().monitor_count(self.target)
    }

    /// How many targets the registry holds `watcher` as watching.
    fn watched_by(&self, watcher: MailboxId) -> usize {
        self.registry.actor_registry().monitoring_count(watcher)
    }
}

/// A WAT guest at the watcher's mailbox whose exports forward to the three
/// watch imports.
struct WatchGuest {
    store: Store<ComponentCtx>,
    watch: TypedFunc<(u64, u64, u64), u64>,
    unwatch: TypedFunc<u64, u32>,
    ended: TypedFunc<(u64, u64, u64), u64>,
}

impl WatchGuest {
    fn at(routes: &Routes) -> Self {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wat = format!(
            r#"
            (module
                (import "aether" "watch_p32" (func $watch (param i64 i64 i64) (result i64)))
                (import "aether" "unwatch_p32" (func $unwatch (param i64) (result i32)))
                (import "aether" "watch_ended_p32" (func $ended (param i64 i64 i64) (result i64)))
                (memory (export "memory") 1)
                {WAT_REALLOC}
                (func (export "watch") (param i64 i64 i64) (result i64)
                    local.get 0
                    local.get 1
                    local.get 2
                    call $watch)
                (func (export "unwatch") (param i64) (result i32)
                    local.get 0
                    call $unwatch)
                (func (export "ended") (param i64 i64 i64) (result i64)
                    local.get 0
                    local.get 1
                    local.get 2
                    call $ended))
            "#
        );
        let module = Module::new(&engine, wat::parse_str(&wat).expect("compile WAT")).expect("compile module");
        let ctx = ctx_at(
            Arc::clone(&routes.registry),
            Arc::clone(&routes.mailer),
            HubOutbound::disconnected(),
            routes.watcher,
            None,
        );
        let mut store = Store::new(&engine, ctx);
        let instance = linker.instantiate(&mut store, &module).expect("instantiate");
        let watch = instance.get_typed_func(&mut store, "watch").expect("watch export");
        let unwatch = instance.get_typed_func(&mut store, "unwatch").expect("unwatch export");
        let ended = instance.get_typed_func(&mut store, "ended").expect("ended export");

        Self { store, watch, unwatch, ended }
    }

    /// Watch `target` as `from` through `watched`, as `watch_p32` answers.
    fn try_watch(&mut self, target: MailboxId, from: MailboxId, watched: u64) -> wasmtime::Result<u64> {
        self.watch.call(&mut self.store, (target.0, from.0, watched))
    }

    fn watch(&mut self, target: MailboxId, from: MailboxId, watched: u64) -> u64 {
        self.try_watch(target, from, watched).expect("watch_p32 answers an id for a routed target")
    }

    fn unwatch(&mut self, watch: u64) -> u32 {
        self.unwatch.call(&mut self.store, watch).expect("unwatch_p32 does not trap")
    }

    /// The watch `watcher` holds on the departed `target` through the watched
    /// type `tag` names, as `watch_ended_p32` answers and ends it.
    fn ended(&mut self, target: MailboxId, watcher: MailboxId, tag: u64) -> u64 {
        self.ended.call(&mut self.store, (target.0, watcher.0, tag)).expect("watch_ended_p32 does not trap")
    }
}

/// Catches a table that does not release on drop: a guest that closed, or
/// trapped and never ran its own code again, would stay registered and its
/// freed mailbox would be sent every later departure.
#[test]
fn dropping_a_guest_that_watched_leaves_both_monitor_indices_empty() {
    let routes = Routes::new();
    let mut guest = WatchGuest::at(&routes);

    assert_ne!(guest.watch(routes.target, routes.watcher, PROVIDER), 0);
    assert_eq!(routes.watchers_of_target(), 1);
    assert_eq!(routes.watched_by(routes.watcher), 1);

    drop(guest);
    assert_eq!(routes.watchers_of_target(), 0);
    assert_eq!(routes.watched_by(routes.watcher), 0);
}

/// Catches a guest watching in another actor's name: a claimed watcher
/// outside the cluster must never be registered, so no other actor is sent a
/// notice it did not ask for, and it ends no watch of this guest's.
#[test]
fn a_claimed_watcher_outside_the_cluster_watches_as_the_guest_itself() {
    let routes = Routes::new();
    let mut guest = WatchGuest::at(&routes);
    let foreign = routes.target;

    let watch = guest.watch(routes.target, foreign, PROVIDER);

    assert_eq!(routes.watched_by(foreign), 0, "the claimed watcher is never registered");
    assert_eq!(routes.watched_by(routes.watcher), 1, "the watch stands under the guest's own mailbox");
    assert_eq!(guest.ended(routes.target, foreign, PROVIDER), 0, "a foreign watcher ends no watch");
    assert_eq!(guest.ended(routes.target, routes.watcher, PROVIDER), watch);
}

/// Catches a registration under a position no reference names: the host
/// must trap rather than watch an arbitrary number a hand-rolled guest
/// passed.
#[test]
fn a_target_with_no_route_record_traps() {
    let routes = Routes::new();
    let mut guest = WatchGuest::at(&routes);

    let refused = guest.try_watch(MailboxId(0xdead_beef), routes.watcher, PROVIDER);

    assert!(refused.is_err(), "a position with no route record is refused");
    assert_eq!(routes.watched_by(routes.watcher), 0);
}

/// Catches a republish handover that copies rows or leaves them behind: the
/// watches must be held by exactly one guest, so the departure is ended by
/// the guest that took them and the retired guest's drop releases nothing.
#[test]
fn watches_moved_to_a_second_guest_are_ended_and_released_by_it_alone() {
    let routes = Routes::new();
    let mut first = WatchGuest::at(&routes);
    let mut second = WatchGuest::at(&routes);
    let watch = first.watch(routes.target, routes.watcher, PROVIDER);
    let kept = first.watch(routes.target, routes.watcher, AUDITOR);

    let moved = first.store.data_mut().take_watches();
    second.store.data_mut().resume_watches(moved);

    assert_eq!(first.ended(routes.target, routes.watcher, PROVIDER), 0, "the first guest holds no row");
    drop(first);
    assert_eq!(routes.watchers_of_target(), 1, "the retired guest's drop releases nothing");

    assert_eq!(second.ended(routes.target, routes.watcher, PROVIDER), watch);
    assert_eq!(second.unwatch(kept), 1, "the moved watch is released under the id its maker was given");
    assert_eq!(routes.watchers_of_target(), 0);

    assert_ne!(second.watch(routes.target, routes.watcher, AUDITOR), kept, "a released id is never handed out again");
    drop(second);
    assert_eq!(routes.watchers_of_target(), 0);
    assert_eq!(routes.watched_by(routes.watcher), 0);
}

/// Catches a second registration for one watcher and target: a repeated
/// watch must answer the standing id, and a second watched type must share
/// the pair's one registry entry, so one departure posts one notice and that
/// notice ends each watched type's watch once.
#[test]
fn one_pair_keeps_one_registration_across_repeats_and_watched_types() {
    let routes = Routes::new();
    let mut guest = WatchGuest::at(&routes);

    let provider = guest.watch(routes.target, routes.watcher, PROVIDER);
    assert_eq!(guest.watch(routes.target, routes.watcher, PROVIDER), provider);
    assert_eq!(routes.watchers_of_target(), 1);

    let auditor = guest.watch(routes.target, routes.watcher, AUDITOR);
    assert_ne!(auditor, provider);
    assert_eq!(routes.watchers_of_target(), 1);

    assert_eq!(guest.ended(routes.target, routes.watcher, PROVIDER), provider);
    assert_eq!(guest.ended(routes.target, routes.watcher, AUDITOR), auditor);
    assert_eq!(guest.ended(routes.target, routes.watcher, PROVIDER), 0, "a watch ends once");
    assert_eq!(guest.unwatch(provider), 0, "an ended watch is not there to release");
    assert_eq!(routes.watchers_of_target(), 0);
}
