use aether_actor::{Addressable, EmbeddedMany, Manual, Many, One, WasmActorMailbox, WasmCtx};
use aether_component::ComponentHostCapability;
use aether_component::component::{ComponentHostNativeExt, ComponentHostWasmExt, PeerCtxExt};
use aether_substrate::actor::native::NativeActorMailbox;

struct RootRecipient;

impl Addressable for RootRecipient {
    const NAMESPACE: &'static str = "test.component.route.root";
    type Resolver = One;
}

struct RelativeRecipient;

impl Addressable for RelativeRecipient {
    const NAMESPACE: &'static str = "test.component.route.relative";
    type Resolver = Many;
}

struct SpawnedRecipient;

impl Addressable for SpawnedRecipient {
    const NAMESPACE: &'static str = "test.component.route.spawned";
    type Resolver = EmbeddedMany;
}

fn rejects_wasm_loaded(host: &WasmActorMailbox<'_, ComponentHostCapability>) {
    let _ = host.loaded::<RootRecipient>("root");
    let _ = host.loaded::<RelativeRecipient>("relative");
    let _ = host.loaded::<SpawnedRecipient>("spawned");
}

fn rejects_native_loaded(host: &NativeActorMailbox<'_, ComponentHostCapability>) {
    let _ = host.loaded::<RootRecipient>("root");
    let _ = host.loaded::<RelativeRecipient>("relative");
    let _ = host.loaded::<SpawnedRecipient>("spawned");
}

fn rejects_peer(ctx: &WasmCtx<'_, Manual>) {
    let _ = ctx.peer::<RootRecipient>();
    let _ = ctx.peer::<RelativeRecipient>();
    let _ = ctx.peer::<SpawnedRecipient>();
    let _ = ctx.peer_named::<RootRecipient>("root");
    let _ = ctx.peer_named::<RelativeRecipient>("relative");
    let _ = ctx.peer_named::<SpawnedRecipient>("spawned");
}

fn main() {}
