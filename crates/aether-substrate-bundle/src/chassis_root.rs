//! Tiny shared helper for the chassis-root mail correlation counter
//! (ADR-0080 §6). Both the headless driver and the test-bench bin
//! own an `AtomicU64` that hands out `correlation_id`s for synthetic
//! chassis-root mail, skipping 0 (reserved sentinel). Extracted from
//! a duplicated closure across both sites — see PR 952's qodana
//! follow-up.

use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically advance `counter` and return the next non-zero id.
/// Symmetric with the per-actor counter on `NativeBinding`; zero
/// stays reserved as the `MailId::NONE` sentinel.
pub fn next_chassis_correlation(counter: &AtomicU64) -> u64 {
    let id = counter.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        counter.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}
