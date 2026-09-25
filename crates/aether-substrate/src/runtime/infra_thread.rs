//! The crate's one door for raw OS threads below the actor and mail layer.
//!
//! A raw spawn bypasses the trace and settlement umbrella (ADR-0080 §12), so
//! handler and capability code spawns through `NativeCtx::spawn_inherit` /
//! `spawn_detached` or the ADR-0093 task system instead. Infrastructure that
//! runs before any actor or beneath them all (the scheduler's workers, its
//! boot calibration probe, the blob store's reclaim thread) holds no binding
//! and no chain, and spawns here: one named thread, nothing inherited.

use std::io;
use std::thread::{self, JoinHandle};

/// Spawn an infrastructure thread named `name` running `body`. It carries no
/// settlement hold and continues no chain, so `body` must send no mail.
///
/// # Errors
///
/// Fails when the OS refuses the thread.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the crate's single infra-thread door below the actor and mail layer; its callers hold no binding and continue no chain
pub fn spawn<F, T>(name: impl Into<String>, body: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new().name(name.into()).spawn(body)
}
