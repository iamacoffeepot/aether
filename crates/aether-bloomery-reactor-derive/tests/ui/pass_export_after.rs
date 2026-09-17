use aether_actor::{actor, export, ActorInitError, WasmActor, WasmCtx, WasmInitCtx};
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{reactor, Output};

#[aether_data::kind(name = "test.bloomery.export.sentinel_out", eq)]
struct Publication { marker: u32 }
impl Output for Publication {}

pub struct Probe;
#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.probe";
    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> { Ok(Probe) }
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}
struct Publisher;
#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.publisher";
    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication { Publication { marker: 1 } }
}
struct Witness;
#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.export.witness";
    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication { Publication { marker: 2 } }
}
macro_rules! InjectSentinel {
    (@aether_export_generate
        { remaining_generators: [$($next:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [
            { ty: { $probe:ty } namespace: $probe_ns:tt extensions: [$($probe_ext:tt)*] }
            { ty: { $publisher:ty } namespace: $publisher_ns:tt extensions: [$($publisher_ext:tt)*] }
            { ty: { $witness:ty } namespace: $witness_ns:tt extensions: [$($witness_ext:tt)*] }
        ], exports: [$($exports:tt)*] }
    ) => {
        aether_actor::__export_continue! {
            remaining_generators: [$($next),*]
            boot: $boot default: $default
            actors: [
                { ty: { $probe } namespace: $probe_ns extensions: [$($probe_ext)* test_export_sentinel { probe }] }
                { ty: { $publisher } namespace: $publisher_ns extensions: [$($publisher_ext)* test_export_sentinel { one }] }
                { ty: { $witness } namespace: $witness_ns extensions: [$($witness_ext)* test_export_sentinel { two }] }
            ]
            exports: [$($exports)*]
        }
    };
}

macro_rules! RequireSentinel {
    (@aether_export_generate
        { remaining_generators: [$($next:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [
            { ty: { $probe:ty } namespace: "test.bloomery.export.probe" extensions: [test_export_sentinel { probe }] }
            { ty: { $publisher:ty } namespace: "test.bloomery.export.publisher" extensions: [aether_bloomery_reactor {} test_export_sentinel { one }] }
            { ty: { $witness:ty } namespace: "test.bloomery.export.witness" extensions: [aether_bloomery_reactor {} test_export_sentinel { two }] }
            { ty: { $coordinator:ty } namespace: "aether.bloomery.reactor" extensions: [] }
        ], exports: [{ $ordinary_export:ty } { $cluster_export:ty } { $peer_a:ty } { $peer_b:ty }] }
    ) => {
        const _: fn() = || {
            use core::marker::PhantomData;
            let _: PhantomData<Probe> = PhantomData::<$probe>;
            let _: PhantomData<Publisher> = PhantomData::<$publisher>;
            let _: PhantomData<Witness> = PhantomData::<$witness>;
            let _: PhantomData<Probe> = PhantomData::<$ordinary_export>;
            let _: PhantomData<$coordinator> = PhantomData::<$cluster_export>;
            let _: PhantomData<$peer_a> = PhantomData::<$peer_a>;
            let _: PhantomData<$peer_b> = PhantomData::<$peer_b>;
        };
        aether_actor::__export_continue! {
            remaining_generators: [$($next),*]
            boot: $boot default: $default
            actors: [
                { ty: { $probe } namespace: "test.bloomery.export.probe" extensions: [test_export_sentinel { probe }] }
                { ty: { $publisher } namespace: "test.bloomery.export.publisher" extensions: [aether_bloomery_reactor {} test_export_sentinel { one }] }
                { ty: { $witness } namespace: "test.bloomery.export.witness" extensions: [aether_bloomery_reactor {} test_export_sentinel { two }] }
                { ty: { $coordinator } namespace: "aether.bloomery.reactor" extensions: [] }
            ]
            exports: [{ $ordinary_export } { $cluster_export } { $peer_a } { $peer_b }]
        }
    };
    ($($unexpected:tt)*) => { compile_error!("actor association, namespace, extension payload, or export selection changed"); };
}
export!(
    default = Probe,
    Publisher,
    Witness,
    generators = [InjectSentinel, aether_bloomery_reactor::bundle_reactors, RequireSentinel],
);
fn main() {}
