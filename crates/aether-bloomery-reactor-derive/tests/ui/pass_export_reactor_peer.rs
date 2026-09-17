// Reactor-only export rewrites to a default coordinator and public peer.

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.reactor_peer_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.reactor_peer";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

macro_rules! RequirePeerFactory {
    (@aether_export_generate
        { remaining_generators: [$($next:path),*] }
        { boot: none, default: { $default:ty }, actors: [
            { ty: { $publisher:ty } namespace: "test.bloomery.export.reactor_peer" extensions: [aether_bloomery_reactor {}] }
            { ty: { $coordinator:ty } namespace: "aether.bloomery.reactor" extensions: [] }
            { ty: { $peer_actor:ty } namespace: "test.bloomery.export.reactor_peer" extensions: [] }
        ], exports: [{ $cluster_export:ty } { $peer:ty }] }
    ) => {
        const _: fn() = || {
            use core::marker::PhantomData;
            let _: PhantomData<Publisher> = PhantomData::<$publisher>;
            let _: PhantomData<$coordinator> = PhantomData::<$cluster_export>;
            let _: PhantomData<$peer> = PhantomData::<$peer>;
            let _: PhantomData<$default> = PhantomData::<$coordinator>;
            let _: PhantomData<$peer_actor> = PhantomData::<$peer>;
        };
        aether_actor::__export_continue! {
            remaining_generators: [$($next),*]
            boot: none default: { $default }
            actors: [
                { ty: { $publisher } namespace: "test.bloomery.export.reactor_peer" extensions: [aether_bloomery_reactor {}] }
                { ty: { $coordinator } namespace: "aether.bloomery.reactor" extensions: [] }
                { ty: { $peer_actor } namespace: "test.bloomery.export.reactor_peer" extensions: [] }
            ]
            exports: [{ $cluster_export } { $peer }]
        }
    };
    ($($unexpected:tt)*) => { compile_error!("reactor-only peer was not a public actor or coordinator default changed"); };
}

export!(Publisher, generators = [aether_bloomery_reactor::bundle_reactors, RequirePeerFactory]);

fn main() {
    let _ = Publisher::NAMESPACE;
    let _ = aether_bloomery_reactor::CLUSTER_NAMESPACE;
}
