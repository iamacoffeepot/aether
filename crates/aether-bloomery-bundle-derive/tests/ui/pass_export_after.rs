use aether_actor::{actor, export, ActorInitError, WasmActor, WasmCtx, WasmInitCtx};
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

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
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead { SetHead::new(&PUBLISHED, None, change.to()) }
}
struct Witness;
#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.export.witness";
    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead { SetHead::new(&PUBLISHED, None, change.to()) }
}
macro_rules! InjectSentinel {
    (@aether_export_generate
        { remaining_generators: [$($next:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [
            { ty: { $probe:ty } namespace: $probe_ns:tt extensions: [$($probe_ext:tt)*] }
            { ty: { $publisher:ty } namespace: $publisher_ns:tt extensions: [$($publisher_ext:tt)*] }
            { ty: { $witness:ty } namespace: $witness_ns:tt extensions: [$($witness_ext:tt)*] }
        ], exports: [$($exports:tt)*], private: [$($private:tt)*] }
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
            private: [$($private)*]
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
            { ty: { $coordinator:ty } namespace: "aether.bloomery.bundle" extensions: [] }
        ], exports: [{ $ordinary_export:ty } { $cluster_export:ty }], private: [$($private:tt)*] }
    ) => {
        const _: fn() = || {
            use core::marker::PhantomData;
            let _: PhantomData<Probe> = PhantomData::<$probe>;
            let _: PhantomData<Publisher> = PhantomData::<$publisher>;
            let _: PhantomData<Witness> = PhantomData::<$witness>;
            let _: PhantomData<Probe> = PhantomData::<$ordinary_export>;
            let _: PhantomData<$coordinator> = PhantomData::<$cluster_export>;
        };
        aether_actor::__export_continue! {
            remaining_generators: [$($next),*]
            boot: $boot default: $default
            actors: [
                { ty: { $probe } namespace: "test.bloomery.export.probe" extensions: [test_export_sentinel { probe }] }
                { ty: { $publisher } namespace: "test.bloomery.export.publisher" extensions: [aether_bloomery_reactor {} test_export_sentinel { one }] }
                { ty: { $witness } namespace: "test.bloomery.export.witness" extensions: [aether_bloomery_reactor {} test_export_sentinel { two }] }
                { ty: { $coordinator } namespace: "aether.bloomery.bundle" extensions: [] }
            ]
            exports: [{ $ordinary_export } { $cluster_export }]
            private: [$($private)*]
        }
    };
    ($($unexpected:tt)*) => { compile_error!("actor association, namespace, extension payload, or export selection changed"); };
}
export!(
    default = Probe,
    public = [Publisher, Witness],
    generators = [InjectSentinel, aether_bloomery_bundle::bundle, RequireSentinel],
);
fn main() {}
