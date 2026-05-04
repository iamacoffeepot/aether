// Tests for `#[actor]` on inherent impls — the native chassis-cap
// path (PR B of issue 533). Verifies:
//
//   - `HandlesKind<K>` impls land for each `#[handler]` method.
//   - `Dispatch::__dispatch(kind: u64, payload: &[u8]) -> Option<()>`
//     routes payloads to the matching handler and returns `None`
//     for unknown kinds.
//
// Compiles native-only — `aether-data-derive` runs as a proc-macro
// crate, so these tests exercise the generated code on the host.

use aether_actor::Actor;
use aether_data::{Dispatch, Kind};
use bytemuck::{Pod, Zeroable};

/// Two distinct cast-shape kinds the test cap handles.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.native_actor.kind_a")]
struct KindA {
    tag: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.native_actor.kind_b")]
struct KindB {
    tag: u32,
}

/// A third kind the cap deliberately doesn't handle — used to verify
/// `__dispatch` returns `None` for unknown kinds.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.native_actor.unhandled")]
struct UnhandledKind {
    tag: u32,
}

/// Backend trait — the substrate-side code impls this; the cap's
/// handler bodies delegate to it. ADR-0075 facade pattern.
trait FacadeBackend: Send + 'static {
    fn on_kind_a(&mut self, k: KindA);
    fn on_kind_b(&mut self, k: KindB);
}

/// Cap struct holding a backend. The `#[actor]` macro emits HandlesKind
/// + __dispatch on this inherent impl.
struct FacadeCap<B: FacadeBackend> {
    backend: B,
}

impl<B: FacadeBackend> Actor for FacadeCap<B> {
    const NAMESPACE: &'static str = "test.facade_cap";
}

#[aether_data::actor]
impl<B: FacadeBackend> FacadeCap<B> {
    #[aether_data::handler]
    fn on_kind_a(&mut self, k: KindA) {
        self.backend.on_kind_a(k);
    }

    #[aether_data::handler]
    fn on_kind_b(&mut self, k: KindB) {
        self.backend.on_kind_b(k);
    }
}

/// Recording backend for the test — captures every handler call so
/// the test can assert `__dispatch` routed to the right method.
#[derive(Default)]
struct RecordingBackend {
    a_calls: Vec<KindA>,
    b_calls: Vec<KindB>,
}
impl FacadeBackend for RecordingBackend {
    fn on_kind_a(&mut self, k: KindA) {
        self.a_calls.push(k);
    }
    fn on_kind_b(&mut self, k: KindB) {
        self.b_calls.push(k);
    }
}

#[test]
fn dispatch_routes_kind_a_to_on_kind_a() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let payload = bytemuck::bytes_of(&KindA { tag: 42 });
    let result = cap.__dispatch(KindA::ID.0, payload);
    assert_eq!(result, Some(()));
    assert_eq!(cap.backend.a_calls.len(), 1);
    assert_eq!(cap.backend.a_calls[0].tag, 42);
    assert!(cap.backend.b_calls.is_empty());
}

#[test]
fn dispatch_routes_kind_b_to_on_kind_b() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let payload = bytemuck::bytes_of(&KindB { tag: 99 });
    let result = cap.__dispatch(KindB::ID.0, payload);
    assert_eq!(result, Some(()));
    assert_eq!(cap.backend.b_calls.len(), 1);
    assert_eq!(cap.backend.b_calls[0].tag, 99);
    assert!(cap.backend.a_calls.is_empty());
}

#[test]
fn dispatch_returns_none_for_unhandled_kind() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let payload = bytemuck::bytes_of(&UnhandledKind { tag: 1 });
    let result = cap.__dispatch(UnhandledKind::ID.0, payload);
    assert_eq!(result, None);
    assert!(cap.backend.a_calls.is_empty());
    assert!(cap.backend.b_calls.is_empty());
}

/// Type-level check: the macro emits `HandlesKind<KindA>` and
/// `HandlesKind<KindB>` for the cap, gating type-driven sends. The
/// fn body never runs — this is purely a trait-bound assertion.
#[test]
fn handles_kind_impls_compile_for_each_handler() {
    fn assert_handles<R: aether_actor::HandlesKind<K>, K: Kind>() {}
    assert_handles::<FacadeCap<RecordingBackend>, KindA>();
    assert_handles::<FacadeCap<RecordingBackend>, KindB>();
}
