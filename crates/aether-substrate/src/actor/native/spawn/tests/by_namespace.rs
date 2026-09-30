//! The link-time [`NativeSpawnEntry`] facts the component host's spawn door
//! decides a native spawn by (ADR-0241 §9), read through
//! [`NativeSpawnEntry::declaring`] exactly as the door reads them.

use std::sync::mpsc::Sender;

use aether_kinds::{Ping, Pong};

use crate::actor::native::spawn::NativeSpawnEntry;
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;

const SINGLETON: &str = "test.by_namespace.singleton";
const ROOTED: &str = "test.by_namespace.rooted";
const CHILD: &str = "test.by_namespace.child";
const WIRED: &str = "test.by_namespace.wired";

struct SingletonFixture;

#[aether_actor::actor(singleton, root)]
impl NativeActor for SingletonFixture {
    const NAMESPACE: &'static str = SINGLETON;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut NativeCtx<'_>, _ping: Ping) -> Pong {
        Pong::default()
    }
}

struct RootedFixture;

#[aether_actor::actor(instanced, root)]
impl NativeActor for RootedFixture {
    const NAMESPACE: &'static str = ROOTED;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut NativeCtx<'_>, _ping: Ping) -> Pong {
        Pong::default()
    }
}

struct ChildFixture;

#[aether_actor::actor(instanced, child_of(SingletonFixture))]
impl NativeActor for ChildFixture {
    const NAMESPACE: &'static str = CHILD;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut NativeCtx<'_>, _ping: Ping) -> Pong {
        Pong::default()
    }
}

/// An instanced type whose `Config` is the wiring its parent hands it.
struct WiredFixture;

#[aether_actor::actor(instanced, root)]
impl NativeActor for WiredFixture {
    const NAMESPACE: &'static str = WIRED;
    type Config = Sender<()>;

    fn init(_wiring: Sender<()>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut NativeCtx<'_>, _ping: Ping) -> Pong {
        Pong::default()
    }
}

fn entry(namespace: &str) -> &'static NativeSpawnEntry {
    let entries: Vec<_> = NativeSpawnEntry::declaring(namespace).collect();
    assert_eq!(entries.len(), 1, "one linked type declares {namespace}");
    assert_eq!(entries[0].namespace(), namespace, "the lookup answers only an entry declaring {namespace}");
    entries[0]
}

#[test]
fn each_entry_reads_its_own_types_declaration() {
    // Catches: a lookup that answers another type's entry, a placement probe
    // whose fallback shadows the `Root` impl (every type reading as unrooted,
    // or as rooted), and a config probe that stages a type whose `Config` is
    // spawn-time wiring, which a spawn by mail has nothing to build from.
    let singleton = entry(SINGLETON);
    let rooted = entry(ROOTED);
    let child = entry(CHILD);
    let wired = entry(WIRED);

    assert!(!singleton.instanced(), "a singleton is composed at boot");
    assert!(!singleton.stageable(), "a singleton is never staged by mail");
    assert!(rooted.instanced() && rooted.declares_root() && rooted.stageable());
    assert!(!rooted.declares_child_of(SINGLETON), "a root-only type declares no parent");
    assert!(child.instanced() && !child.declares_root(), "a child-only type is not placed at the root");
    assert!(child.declares_child_of(SINGLETON) && !child.declares_child_of(ROOTED));
    assert!(wired.instanced() && wired.declares_root());
    assert!(!wired.stageable(), "a type whose Config is wiring is not staged by mail");
}
