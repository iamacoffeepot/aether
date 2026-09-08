//! The chassis-owned handle map: publish, retrieve, and the bounds that
//! make a published handle safe to read from a driver thread.

use std::any::Any;
use std::any::TypeId;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::actor::native::ExportedHandles;

use super::support::{StubActor, StubHandles};

#[test]
fn handles_insert_and_get_roundtrip() {
    let mut handles = ExportedHandles::new();
    assert!(handles.is_empty());
    let counter = Arc::new(AtomicU32::new(0));
    handles.by_type.insert(TypeId::of::<StubHandles>(), Box::new(StubHandles { counter: Arc::clone(&counter) }));
    assert_eq!(handles.len(), 1);

    let retrieved: StubHandles = handles.get::<StubHandles>().expect("StubHandles published");
    retrieved.counter.fetch_add(1, Ordering::SeqCst);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn handles_get_returns_none_for_unpublished_type() {
    let handles = ExportedHandles::new();
    assert!(handles.get::<StubHandles>().is_none());
}

/// Compile-time signal that `NativeActor` is `Send + 'static` (no
/// `Sync`), and that `ExportedHandles` values are
/// `Send + Sync + Clone` so cross-thread driver access works.
/// If a future change to `Addressable` drops `Send + 'static`, the
/// asserts here fail to instantiate.
fn _assert_actor_send() {
    fn requires_send<T: Send + 'static>() {}
    fn requires_handle<H: Any + Send + Sync + Clone + 'static>() {}
    requires_send::<StubActor>();
    requires_handle::<StubHandles>();
}
