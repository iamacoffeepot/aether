//! Pre-Phase-4 input-subscription round-trip test (ADR-0021).
//! Drove `subscribe` / `unsubscribe` against the cap's
//! `for_test` / `load_for_test` helpers and used `mailer.drain_all`
//! to synchronise.
//!
//! Issue 634 Phase 4 PR 1 retired both surfaces — the cap's
//! synthetic load helpers along with the dispatcher-side
//! drain barrier. This test needs rewriting against
//! `aether-substrate-bundle::test_bench::TestBench` so the
//! subscribe/unsubscribe flow runs through a real chassis. Tracked
//! under issue 648.

#![cfg(any())]
