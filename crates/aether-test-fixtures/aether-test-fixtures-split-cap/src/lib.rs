//! ADR-0123 struct-hosted split-cap fixture.
//!
//! The always-on *identity* surface lives here: the kind vocabulary, the
//! capability ZST, and the markers `#[actor]` lifts. The behaviour + runtime
//! state live in `runtime.rs`, gated behind `feature = "runtime"`.
//! `#[actor(singleton)]` on the struct reads `runtime.rs` off disk and lifts
//! the identity (`Addressable` + per-handler `HandlesKind<K>` + the
//! name-inventory entry) up to here, so it survives a `--no-default-features`
//! build where `mod runtime` is `#[cfg]`-stripped.

use aether_actor::actor;

/// A cast-shaped mail kind the cap handles. `Pod`/`Zeroable` so it needs no
/// serde; defined here in the identity so the lifted `HandlesKind<Ping>` marker
/// resolves feature-off.
#[repr(C)]
#[derive(
    Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.split_cap.ping")]
pub struct Ping {
    pub seq: u32,
}

/// A second handled kind, so the lift emits more than one `HandlesKind` marker.
#[repr(C)]
#[derive(
    Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.split_cap.pong")]
pub struct Pong {
    pub seq: u32,
}

/// The capability identity ZST. `#[actor(singleton)]` reads the sibling
/// `runtime` module off disk and lifts `impl Addressable` (`NAMESPACE` from the
/// runtime impl's const, `Resolver = One` from `singleton`) plus one
/// `impl HandlesKind<K>` per `#[handler]` here — all always-on. An explicit
/// `#[actor(singleton, other_module)]` would override the module name.
#[actor(singleton)]
pub struct SplitCap;

#[cfg(feature = "runtime")]
mod runtime;

// Proof the identity survives a feature-off build. The turbofish forces rustc
// to prove the bounds at this site; the body is type-checked even though it is
// never called. Compiling with `--no-default-features` — where `mod runtime` is
// stripped — means the `Addressable` + `HandlesKind<_>` impls came only from
// `#[actor]`'s disk read.
#[allow(dead_code)]
fn assert_identity_present() {
    fn requires<T>()
    where
        T: aether_actor::HandlesKind<Ping>
            + aether_actor::HandlesKind<Pong>
            + aether_actor::Addressable<Resolver = aether_actor::One>,
    {
    }
    requires::<SplitCap>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_actor::Addressable;

    // Runs in both feature configs (the test lib compiles ungated). Proves the
    // namespace `#[actor]` lifted is the value read from the runtime impl's
    // const, even when `mod runtime` itself is stripped.
    #[test]
    fn namespace_lifted_from_runtime_const() {
        assert_eq!(<SplitCap as Addressable>::NAMESPACE, "test.split_cap");
    }

    // Behaviour survives the split: only meaningful (and compiled) with the
    // runtime feature, where `#[runtime]` emitted the `Lifecycle` impl carrying
    // `init` over the declared state. The bound proof forces rustc to confirm
    // the impl exists without standing up a chassis to call `init`.
    #[cfg(feature = "runtime")]
    #[test]
    fn runtime_lifecycle_present() {
        fn requires<T: aether_actor::Lifecycle<runtime::SplitCapState>>() {}
        requires::<SplitCap>();
    }

    // The runtime state is plain data the dispatch surface runs over.
    #[cfg(feature = "runtime")]
    #[test]
    fn runtime_state_is_plain_data() {
        let state = runtime::SplitCapState { pings: 0, pongs: 0 };
        assert_eq!(state.pings + state.pongs, 0);
    }
}
