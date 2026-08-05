use aether_actor::{ActorInitError, Addressable, ChildOf, One, WasmActor, WasmInitCtx, actor};

struct FirstParent;

impl Addressable for FirstParent {
    const NAMESPACE: &'static str = "test.lineage.first";
    type Resolver = One;
}

struct SecondParent;

impl Addressable for SecondParent {
    const NAMESPACE: &'static str = "test.lineage.second";
    type Resolver = One;
}

struct PlacedActor;

#[actor(instanced, child_of(FirstParent), child_of(SecondParent))]
impl WasmActor for PlacedActor {
    const NAMESPACE: &'static str = "test.lineage.placed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

fn main() {
    fn first<T: ChildOf<FirstParent>>() {}
    fn second<T: ChildOf<SecondParent>>() {}
    first::<PlacedActor>();
    second::<PlacedActor>();
    assert!(PlacedActor::__AETHER_LINEAGE_MANIFEST_LEN > 0);
}
