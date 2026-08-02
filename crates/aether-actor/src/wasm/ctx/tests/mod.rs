//! Host-build unit tests for the wasm ctx family, and the fixture actors
//! they share. The fixtures live here so both test modules reach them
//! through `super::`; the assertions split by subject — `spawn` for child
//! creation and teardown, `dispatch` for what a ctx reads off the dispatch
//! it was built for.

mod dispatch;
mod spawn;

use super::{ActorTypeTag, NO_INBOUND_SOURCE, SpawnError, WasmCtx, install_inline_child};
use crate::mail::{Mail, PriorState};
use crate::model::Subname;
use crate::model::ctx::Manual;
use crate::wasm::inline::Registry;
use crate::wasm::inline::compose::spawn_one_child;
use crate::wasm::{
    __validate_inline_child_placement, ActorInitError, ErasedWasmActor, WasmActor, WasmDropCtx, WasmInitCtx,
    WasmPlacementFacts,
};
use crate::{Addressable, ChildOf, HandlesKind, ModuleChild};
use aether_data::{Kind, MailboxId};
use alloc::string::String;
use core::cell::Cell;

/// Test inline child whose `init` always fails — drives the
/// [`SpawnError::InitFailed`] path. The `ErasedWasmActor` dispatch
/// hooks are unreachable: a failed `init` never registers or
/// dispatches the child.
struct FailingChild;

impl Addressable for FailingChild {
    const NAMESPACE: &'static str = "test.inline.failing_child";
    type Resolver = crate::Many;
}

impl crate::Lifecycle<Self> for FailingChild {
    type Config = ();
    type Params = ();
    type InitError = ActorInitError;
    type InitCtx<'a> = WasmInitCtx<'a>;
    type Ctx<'a> = WasmCtx<'a>;

    fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Err(ActorInitError::new("inline child init deliberately fails"))
    }
}

impl WasmActor for FailingChild {
    type State = Self;
    type Persist = ();
}

impl crate::WasmDispatch<Self> for FailingChild {
    fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("a failed-init child is never dispatched")
    }
}

impl ErasedWasmActor for FailingChild {
    fn erased_namespace(&self) -> &'static str {
        Self::NAMESPACE
    }
    fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("a failed-init child is never dispatched")
    }
    fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
        unreachable!()
    }
    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
        unreachable!()
    }
    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {
        unreachable!()
    }
    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {
        unreachable!()
    }
}

/// Test inline child whose `init` succeeds, so `install_inline_child`
/// registers it in the test-local registry for the despawn test. Its
/// dispatch hooks are unreachable here — the test only installs then
/// despawns.
struct SucceedingChild;

impl Addressable for SucceedingChild {
    const NAMESPACE: &'static str = "test.inline.succeeding_child";
    type Resolver = crate::Many;
}

impl crate::Lifecycle<Self> for SucceedingChild {
    type Config = ();
    type Params = ();
    type InitError = ActorInitError;
    type InitCtx<'a> = WasmInitCtx<'a>;
    type Ctx<'a> = WasmCtx<'a>;

    fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }
}

impl WasmActor for SucceedingChild {
    type State = Self;
    type Persist = ();
}

impl ModuleChild for SucceedingChild {}

impl SucceedingChild {
    const __AETHER_PLACEMENT: WasmPlacementFacts =
        WasmPlacementFacts { is_instanced: true, module_child: true, exact_parent_tags: &[] };
}

impl crate::WasmDispatch<Self> for SucceedingChild {
    fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the despawn test never dispatches this child")
    }
}

impl HandlesKind<()> for SucceedingChild {}

impl ErasedWasmActor for SucceedingChild {
    fn erased_namespace(&self) -> &'static str {
        Self::NAMESPACE
    }
    fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the despawn test never dispatches this child")
    }
    fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
}

// Issue 2692: the by-tag spawn host-unit fixtures. `thread_local` (not
// a `static`) keeps parallel-threaded test runs from racing on the
// observed-config cell.
extern crate std;

std::thread_local! {
    /// The `value` field [`StubChild::init`] last decoded from its
    /// config bytes, so the by-tag spawn test can assert the passed
    /// bytes were threaded through decode → init.
    static STUB_INIT_CONFIG: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Config for [`StubChild`] carrying an observable `value`, so a by-tag
/// spawn test proves `config_bytes` were decoded and handed to `init`
/// (rather than dropped or replaced with an empty default).
#[derive(::aether_data::Kind, ::aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Default)]
#[kind(name = "test.inline.stub_config")]
struct StubConfig {
    value: u32,
}

/// Inline child whose `init` records its decoded config `value` into the
/// thread-local, so the by-tag host-unit test reads back what was
/// threaded. Its dispatch / lifecycle hooks are unreachable — the tests
/// only spawn it, never mail it.
struct StubChild;

impl Addressable for StubChild {
    const NAMESPACE: &'static str = "test.inline.stub_child";
    type Resolver = crate::Many;
}

impl crate::Lifecycle<Self> for StubChild {
    type Config = StubConfig;
    type Params = ();
    type InitError = ActorInitError;
    type InitCtx<'a> = WasmInitCtx<'a>;
    type Ctx<'a> = WasmCtx<'a>;

    fn init(config: StubConfig, _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        STUB_INIT_CONFIG.set(Some(config.value));
        Ok(Self)
    }
}

impl WasmActor for StubChild {
    type State = Self;
    type Persist = ();
}

impl StubChild {
    const __AETHER_PLACEMENT: WasmPlacementFacts = WasmPlacementFacts {
        is_instanced: true,
        module_child: false,
        exact_parent_tags: &[ActorTypeTag::of::<NestingParent>()],
    };
}

impl crate::WasmDispatch<Self> for StubChild {
    fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the by-tag spawn tests never dispatch the stub child")
    }
}

impl ErasedWasmActor for StubChild {
    fn erased_namespace(&self) -> &'static str {
        Self::NAMESPACE
    }
    fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the by-tag spawn tests never dispatch the stub child")
    }
    fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
}

/// Synthetic stand-in for the `export!`-generated resolver: matches the
/// [`StubChild`] tag against the (one-type) exported set and, on a
/// match, fabricates the alias the real macro resolver would have
/// allocated via the host `spawn_inline_child` host fn (which panics on
/// the host build) before running the shared `spawn_one_child` core. Any
/// other tag falls through to [`SpawnError::UnknownActorTag`], exactly
/// as the generated resolver's tag-match fall-through does.
fn stub_resolver(
    registry: &Registry,
    parent: u64,
    tag: ActorTypeTag,
    is_counter: bool,
    full_subname: &str,
    config_bytes: &[u8],
) -> Result<MailboxId, SpawnError> {
    if tag == ActorTypeTag::of::<StubChild>() {
        __validate_inline_child_placement(registry, parent, tag, StubChild::__AETHER_PLACEMENT)?;
        let alias = MailboxId(0xABCD_0001);
        spawn_one_child::<StubChild>(
            registry,
            parent,
            alias,
            tag.0,
            String::from(full_subname),
            is_counter,
            config_bytes,
        )
    } else {
        Err(SpawnError::UnknownActorTag(tag))
    }
}

/// A resolver that panics if reached — the subname-validation-first
/// tests install it to prove the guard runs before any resolver call.
fn panicking_resolver(
    _registry: &Registry,
    _parent: u64,
    _tag: ActorTypeTag,
    _is_counter: bool,
    _full_subname: &str,
    _config_bytes: &[u8],
) -> Result<MailboxId, SpawnError> {
    panic!("the resolver must not run when subname validation fails")
}

std::thread_local! {
    /// How many times a [`LifecycleProbe`] has run its `wire`, so the
    /// spawn-runs-`wire` and reconstruct-does-not-`wire` tripwires can
    /// observe the lifecycle call (issue 2746).
    static PROBE_WIRE_COUNT: Cell<u32> = const { Cell::new(0) };
    /// How many times a [`LifecycleProbe`] has run its `unwire`, so the
    /// despawn-runs-`unwire` tripwire can observe the teardown call.
    static PROBE_UNWIRE_COUNT: Cell<u32> = const { Cell::new(0) };
}

/// Inline child whose `wire` / `unwire` bump thread-local counters, so
/// the composition path's new lifecycle calls (issue 2746) are
/// observable. Its dispatch hook is unreachable — the tests only spawn /
/// despawn / reconstruct it.
struct LifecycleProbe;

impl Addressable for LifecycleProbe {
    const NAMESPACE: &'static str = "test.inline.lifecycle_probe";
    type Resolver = crate::Many;
}

impl crate::Lifecycle<Self> for LifecycleProbe {
    type Config = ();
    type Params = ();
    type InitError = ActorInitError;
    type InitCtx<'a> = WasmInitCtx<'a>;
    type Ctx<'a> = WasmCtx<'a>;

    fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }
}

impl WasmActor for LifecycleProbe {
    type State = Self;
    type Persist = ();
}

impl crate::WasmDispatch<Self> for LifecycleProbe {
    fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the lifecycle-probe tests never dispatch this child")
    }
}

impl ErasedWasmActor for LifecycleProbe {
    fn erased_namespace(&self) -> &'static str {
        Self::NAMESPACE
    }
    fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the lifecycle-probe tests never dispatch this child")
    }
    fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
        PROBE_WIRE_COUNT.set(PROBE_WIRE_COUNT.get() + 1);
    }
    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
        PROBE_UNWIRE_COUNT.set(PROBE_UNWIRE_COUNT.get() + 1);
    }
    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
}

/// Inline child whose `wire` spawns a nested inline child by tag — the
/// reentrant shape the take/reinsert composition must support (a `wire`
/// that re-enters the registry to install a grandchild). `BehaviorHost`'s
/// `wire` spawns its wrapped widget exactly this way in the live engine
/// (issue 2746). The nested child is a [`StubChild`], resolved through
/// [`stub_resolver`], so its `init` records the threaded config.
struct NestingParent;

impl Addressable for NestingParent {
    const NAMESPACE: &'static str = "test.inline.nesting_parent";
    type Resolver = crate::Many;
}

impl crate::Lifecycle<Self> for NestingParent {
    type Config = ();
    type Params = ();
    type InitError = ActorInitError;
    type InitCtx<'a> = WasmInitCtx<'a>;
    type Ctx<'a> = WasmCtx<'a>;

    fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }
}

impl WasmActor for NestingParent {
    type State = Self;
    type Persist = ();
}

impl ChildOf<NestingParent> for FailingChild {}
impl ChildOf<NestingParent> for StubChild {}

impl crate::WasmDispatch<Self> for NestingParent {
    fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the nesting-parent test never dispatches this child")
    }
}

impl ErasedWasmActor for NestingParent {
    fn erased_namespace(&self) -> &'static str {
        Self::NAMESPACE
    }
    fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
        unreachable!("the nesting-parent test never dispatches this child")
    }
    fn erased_wire(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let config_bytes = StubConfig { value: 0x0BAD_CAFE }.encode_into_bytes();
        ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("nested"), &config_bytes)
            .expect("the nested by-tag spawn during wire succeeds");
    }
    fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
    fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
    fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
}
