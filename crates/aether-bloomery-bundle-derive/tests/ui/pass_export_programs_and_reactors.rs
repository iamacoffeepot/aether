// Catches a mixed module that emits two roots, drops one role, or leaves a program or reactor in `exports`.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{Head, HeadMoved, Mode, Refusal, SetHead, Tree};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.mixed_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.one.input")]
struct OneIn {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.pass.one.result")]
struct OneOut {
    n: u32,
}

struct One;

#[program]
impl Program for One {
    const NAME: &'static str = "test.program.one";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "First program in a two-program bundle.";
    type Input = OneIn;
    type Result = OneOut;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(OneOut { n: input.n })
    }
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

macro_rules! RequireOneRoot {
    (@aether_export_generate
        { remaining_generators: [$($next:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [
            { ty: { $probe:ty } namespace: $probe_ns:tt extensions: [$($probe_ext:tt)*] }
            { ty: { $one:ty } namespace: $one_ns:tt extensions: [$($one_ext:tt)*] }
            { ty: { $publisher:ty } namespace: $publisher_ns:tt extensions: [$($publisher_ext:tt)*] }
            { ty: { $root:ty } namespace: "aether.bloomery.bundle" extensions: [] }
        ], exports: [{ $ordinary:ty } { $bundle:ty }], private: [{ $invocation:ty }] }
    ) => {
        const _: fn() = || {
            use core::marker::PhantomData;
            let _: PhantomData<Probe> = PhantomData::<$ordinary>;
            let _: PhantomData<$root> = PhantomData::<$bundle>;
        };
        aether_actor::__export_continue! {
            remaining_generators: [$($next),*]
            boot: $boot default: $default
            actors: [
                { ty: { $probe } namespace: $probe_ns extensions: [$($probe_ext)*] }
                { ty: { $one } namespace: $one_ns extensions: [$($one_ext)*] }
                { ty: { $publisher } namespace: $publisher_ns extensions: [$($publisher_ext)*] }
                { ty: { $root } namespace: "aether.bloomery.bundle" extensions: [] }
            ]
            exports: [{ $ordinary } { $bundle }]
            private: [{ $invocation }]
        }
    };
    ($($unexpected:tt)*) => {
        compile_error!("expected one bundle root at aether.bloomery.bundle plus the ordinary export");
    };
}

export!(default = Probe, One, Publisher, generators = [aether_bloomery_bundle::bundle, RequireOneRoot]);

fn main() {}
