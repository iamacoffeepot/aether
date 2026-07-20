//! The residue of the native chassis-capability crate (issue 552 stage
//! 2e), now down to a single stub cap.
//!
//! Every real capability that lived here has been extracted to its own
//! per-cap crate by the arc that dissolves this monolith — the `http`
//! client and server were the last, moving to `aether-http` and
//! `aether-http-derive` (iamacoffeepot/aether#3758). What remains is
//! [`test_bench`], the `aether.test_bench` fail-fast stub that desktop and
//! headless compose so `aether.test_bench.advance` mail errors instead of
//! warn-dropping into a hung reply slot.

pub mod test_bench;

pub use test_bench::UnsupportedTestBenchCapability;
