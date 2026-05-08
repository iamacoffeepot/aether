//! Pre-Phase-4 end-to-end test: WAT-built component →
//! `SubstrateCtx::send` → sink handler. Used `ComponentHostCapability`'s
//! `for_test` / `attach_component_for_test` test helpers and the
//! mailer's `drain_all` to drive a hand-built `Component` past the
//! cap's dispatcher infrastructure.
//!
//! Issue 634 Phase 4 PR 1 retired the dispatcher infrastructure
//! along with the cap-side test helpers. The trampoline migration
//! moves component routing onto the framework's `NativeActor`
//! dispatcher; there's no path to attach a hand-built `Component`
//! synchronously anymore. This test needs rewriting against
//! `aether-substrate-bundle::test_bench::TestBench` (in-process
//! chassis driver) — tracked under issue 648.

#![cfg(any())]
