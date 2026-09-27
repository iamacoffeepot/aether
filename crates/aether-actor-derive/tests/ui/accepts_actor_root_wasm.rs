use aether_actor::{ActorInitError, Addressable, ChildOf, One, WasmActor, WasmInitCtx, actor};

struct Parent;

impl Addressable for Parent {
    const NAMESPACE: &'static str = "test.root.parent";
    type Resolver = One;
}

struct DefaultRoot;

#[actor(root)]
impl WasmActor for DefaultRoot {
    const NAMESPACE: &'static str = "test.root.default";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct InstancedRoot;

#[actor(instanced, root)]
impl WasmActor for InstancedRoot {
    const NAMESPACE: &'static str = "test.root.instanced";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct PlacedRoot;

#[actor(instanced, root, child_of(Parent))]
impl WasmActor for PlacedRoot {
    const NAMESPACE: &'static str = "test.root.placed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

fn main() {
    fn child<T: ChildOf<Parent>>() {}
    child::<PlacedRoot>();

    // Each `root` writes a lineage record whatever the cardinality; an
    // instanced root with no other placement has nothing else to write.
    assert!(DefaultRoot::__AETHER_LINEAGE_MANIFEST_LEN > 0);
    assert!(InstancedRoot::__AETHER_LINEAGE_MANIFEST_LEN > 0);
    assert!(PlacedRoot::__AETHER_LINEAGE_MANIFEST_LEN > InstancedRoot::__AETHER_LINEAGE_MANIFEST_LEN);
}
