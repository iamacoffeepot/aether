//! Spike consumer — the identity/runtime split with `#[actor]` on the cap.
//!
//! The always-on *identity* surface lives here: the marker traits, the kind
//! vocabulary, the cap ZST. The behavior + state live in `runtime.rs`, gated
//! behind `feature = "runtime"`. `#[actor(singleton, runtime)]` reads
//! `runtime.rs` off disk and lifts the identity (`Addressable` + `Handles<K>`)
//! up to here, so it survives a `--no-default-features` build where
//! `mod runtime` is stripped.

/// Always-on addressing marker — stand-in for `aether_actor::HandlesKind<K>`.
pub trait Handles<K> {}

/// Cardinality resolvers — `One` (singleton) / `Many` (instanced), the
/// `Addressable::Resolver` stand-in.
pub struct One;
pub struct Many;

/// Addressing identity — stand-in for `aether_actor::Addressable`.
pub trait Addressable {
    const NAMESPACE: &'static str;
    type Resolver;
}

/// Behavior surface — stand-in for the gated `Lifecycle`/`Dispatch`/
/// `NativeActor` impls. `init` is the lifecycle the split must not lose.
pub trait Runtime: Sized {
    type State;
    fn init() -> Self::State;
}

/// Always-on kind vocabulary.
pub struct Tick;
pub struct Resize;

/// The cap identity ZST. `#[actor(singleton)]` defaults to the sibling
/// `runtime` module — reads `runtime.rs`, lifts `impl Addressable` (namespace
/// from the impl's const, `Resolver = One` from `singleton`) and
/// `impl Handles<Tick|Resize>` here — all always-on. An explicit
/// `#[actor(singleton, other_module)]` would override the module name.
#[pull_up_macro::actor(singleton)]
pub struct RenderCapability;

#[cfg(feature = "runtime")]
mod runtime;

// Proof the identity survives a feature-off build. The turbofish call forces
// rustc to *prove* the bounds at this site; the body is type-checked even
// though never called. Compiling with `--no-default-features` (where
// `mod runtime` is stripped) means the `Addressable` + `Handles<_>` impls came
// only from `#[actor]`'s disk read.
#[allow(dead_code)]
fn _assert_identity_present() {
    fn requires<T: Handles<Tick> + Handles<Resize> + Addressable<Resolver = One>>() {}
    requires::<RenderCapability>();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Runs in both feature configs (the test lib compiles ungated). Proves the
    // namespace `#[actor]` lifted is the value read from runtime.rs's const,
    // even when `mod runtime` itself is stripped.
    #[test]
    fn namespace_lifted_from_runtime_const() {
        assert_eq!(<RenderCapability as Addressable>::NAMESPACE, "spike.render");
    }

    // Lifecycle survives the split: only meaningful (and compiled) with the
    // runtime feature, where `#[runtime]` emitted the behavior impl.
    #[cfg(feature = "runtime")]
    #[test]
    fn lifecycle_init_runs() {
        let state = <RenderCapability as Runtime>::init();
        assert_eq!(state.frames, 0);
    }
}
