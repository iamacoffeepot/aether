// These builder tests are deliberate embedders: they exercise `Builder::new`
// directly (it is the unit under test) rather than the `composed` boot seam.
#![allow(clippy::disallowed_methods)]

// ADR-0156 §3: these builder-test caps carry shared counters / a raw sentinel
// as construction wiring, not operator config. The composition boundary bounds
// `Config` by `ConfigMember`, so each cap keeps `Config = ()` and moves its
// wiring onto the `Params` channel (#3851) — its semantically correct home —
// rather than stamp an empty member impl (which the required-method `members`
// forbids by design).

#[macro_use]
mod support;

mod actor_cost;
mod claim;
mod driver;
mod inline_child_alias;
mod monitor;
mod options;
mod resolve;
mod seal;
mod singleton_boot;
mod spawn_child;
mod spawn_instanced;
mod teardown;
mod wire_pass;
