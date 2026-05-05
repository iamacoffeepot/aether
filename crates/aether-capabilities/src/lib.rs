//! Native chassis capabilities (issue 552 stage 2e). Each module
//! implements one of the substrate's chassis-policy mailboxes as a
//! [`NativeActor`] — owning its mailbox name, state, and handlers.
//! The `Builder::with_actor` boot path on `aether-substrate` is the
//! installation site; chassis mains pick which caps to load
//! (Log/Handle/Io/Net are universal; Audio + Render gate behind the
//! `audio` / `render` features).
//!
//! Pre-stage-2e these modules lived under
//! `aether_substrate::capabilities`. The split decouples the
//! cap-marker layer from the substrate runtime so wasm components
//! can address caps via `ctx.send_to::<R>` (resolved through
//! `R::NAMESPACE`) without dragging in wasmtime / wgpu / cpal. Today
//! the crate always pulls `aether-substrate` (the `NativeActor`
//! impls live alongside the structs); the header-only wasm build is
//! a follow-up.
//!
//! [`HubBroadcast`] is a synthetic actor — there is no
//! `NativeActor` impl, just an [`Actor`] marker. The actual
//! broadcast sink is registered as a closure in
//! `aether_substrate::SubstrateBoot::build`. The marker exists so
//! typed-send-to-broadcast (Stage 3) compiles against the same
//! `R: HandlesKind<K>` shape every other actor uses once the
//! relevant `HandlesKind<K>` impls are wired.
//!
//! [`NativeActor`]: aether_substrate::native_actor::NativeActor
//! [`Actor`]: aether_actor::Actor

#[cfg(feature = "audio")]
pub mod audio;
pub mod broadcast;
pub mod handle;
pub mod io;
pub mod log;
pub mod net;
#[cfg(feature = "render")]
pub mod render;

#[cfg(feature = "audio")]
pub use audio::{AudioCapability, AudioConfig};
pub use broadcast::HubBroadcast;
pub use handle::HandleCapability;
pub use io::IoCapability;
pub use log::LogCapability;
pub use net::{NetCapability, NetConfig};
#[cfg(feature = "render")]
pub use render::{RenderCapability, RenderConfig, RenderGpu, RenderHandles};
