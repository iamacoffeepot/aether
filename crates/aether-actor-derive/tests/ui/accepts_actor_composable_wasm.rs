use aether_actor::{
    ActorInitError, ActorTypeTag, ChildOf, Instanced, ModuleChild, WasmActor, WasmCtx, WasmInitCtx, actor,
};

struct FirstParent;

#[actor]
impl WasmActor for FirstParent {
    const NAMESPACE: &'static str = "test.composable.first";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct SecondParent;

#[actor]
impl WasmActor for SecondParent {
    const NAMESPACE: &'static str = "test.composable.second";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct ReusableChild;

#[actor(instanced, composable)]
impl WasmActor for ReusableChild {
    const NAMESPACE: &'static str = "test.composable.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

fn main() {
    fn module_child<T: ModuleChild>() {}
    fn instanced<T: Instanced>() {}
    fn child_of_first<T: ChildOf<FirstParent>>() {}
    fn child_of_second<T: ChildOf<SecondParent>>() {}

    module_child::<ReusableChild>();
    instanced::<ReusableChild>();
    child_of_first::<ReusableChild>();
    child_of_second::<ReusableChild>();

    let facts = ReusableChild::__AETHER_PLACEMENT;
    let _: &[ActorTypeTag] = facts.exact_parent_tags;
    assert!(facts.is_instanced);
    assert!(facts.module_child);
    assert!(ReusableChild::__AETHER_LINEAGE_MANIFEST_LEN > 0);
}
