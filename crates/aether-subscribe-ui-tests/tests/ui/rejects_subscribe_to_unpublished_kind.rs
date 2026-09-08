use aether_actor::WasmActorMailbox;
use aether_kinds::{Key, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

fn rejects_window_stage_subscription(windows: &WasmActorMailbox<'_, WindowCapability>) {
    windows.subscribe::<Tick>(WindowSelector::All);
}

fn rejects_lifecycle_device_subscription(lifecycle: &WasmActorMailbox<'_, LifecycleCapability>) {
    lifecycle.subscribe::<Key>();
}

fn main() {}
