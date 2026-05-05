// Tests for `#[actor]` on inherent impls — the native chassis-cap
// path (issue 533 PR B + PR D1). Verifies:
//
//   - `HandlesKind<K>` impls land for each `#[handler]` method.
//   - `Dispatch::__dispatch(sender, kind, payload) -> Option<()>`
//     routes payloads to the matching handler and returns `None`
//     for unknown kinds.
//   - Both 2-arg `(&mut self, K)` and 3-arg
//     `(&mut self, sender: ReplyTo, K)` handler signatures are
//     supported (issue 533 PR D1).
//
// Compiles native-only — `aether-actor-derive` runs as a proc-macro
// crate, so these tests exercise the generated code on the host.

use aether_actor::{Actor, Dispatch};
use aether_data::{Kind, ReplyTarget, ReplyTo, SessionToken, Uuid};
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
///
/// `on_kind_b` takes a `sender` to exercise the 3-arg `#[handler]`
/// signature added in issue 533 PR D1; `on_kind_a` keeps the
/// fire-and-forget 2-arg shape so both paths are covered in one
/// fixture.
trait FacadeBackend: Send + 'static {
    fn on_kind_a(&mut self, k: KindA);
    fn on_kind_b(&mut self, sender: ReplyTo, k: KindB);
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
    /// 2-arg native handler — sender ignored.
    #[aether_data::handler]
    fn on_kind_a(&mut self, k: KindA) {
        self.backend.on_kind_a(k);
    }

    /// 3-arg native handler — sender forwarded to the backend.
    #[aether_data::handler]
    fn on_kind_b(&mut self, sender: ReplyTo, k: KindB) {
        self.backend.on_kind_b(sender, k);
    }
}

/// Recording backend for the test — captures every handler call so
/// the test can assert `__dispatch` routed to the right method.
#[derive(Default)]
struct RecordingBackend {
    a_calls: Vec<KindA>,
    b_calls: Vec<(ReplyTo, KindB)>,
}
impl FacadeBackend for RecordingBackend {
    fn on_kind_a(&mut self, k: KindA) {
        self.a_calls.push(k);
    }
    fn on_kind_b(&mut self, sender: ReplyTo, k: KindB) {
        self.b_calls.push((sender, k));
    }
}

fn session_sender(token: u128) -> ReplyTo {
    ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(token))))
}

#[test]
fn dispatch_routes_kind_a_to_on_kind_a() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let payload = bytemuck::bytes_of(&KindA { tag: 42 });
    let result = cap.__dispatch(ReplyTo::NONE, KindA::ID.0, payload);
    assert_eq!(result, Some(()));
    assert_eq!(cap.backend.a_calls.len(), 1);
    assert_eq!(cap.backend.a_calls[0].tag, 42);
    assert!(cap.backend.b_calls.is_empty());
}

#[test]
fn dispatch_routes_kind_b_to_on_kind_b_with_sender() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let sender = session_sender(0xfeed_beef);
    let payload = bytemuck::bytes_of(&KindB { tag: 99 });
    let result = cap.__dispatch(sender, KindB::ID.0, payload);
    assert_eq!(result, Some(()));
    assert_eq!(cap.backend.b_calls.len(), 1);
    let (recorded_sender, recorded_kind) = cap.backend.b_calls[0];
    assert_eq!(recorded_sender, sender);
    assert_eq!(recorded_kind.tag, 99);
    assert!(cap.backend.a_calls.is_empty());
}

#[test]
fn dispatch_returns_none_for_unhandled_kind() {
    let mut cap = FacadeCap {
        backend: RecordingBackend::default(),
    };
    let payload = bytemuck::bytes_of(&UnhandledKind { tag: 1 });
    let result = cap.__dispatch(ReplyTo::NONE, UnhandledKind::ID.0, payload);
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
