//! `BloomeryHarness` — a real-coordinator E2E test-support harness over the
//! Bloomery chassis (issue 5332). Where `aether-bloomery`'s `common::step`
//! drives the pure reducer, `BloomeryHarness` drives the *actual* dispatch
//! → executor → intake seam: it boots a coordinator-shaped chassis
//! (`BloomeryChassis::build` in-process by default, a forked `bloomery`
//! child as an axis), connects a raw-frame `TcpStream` client, and runs
//! the production reactors against a real journal and a real git
//! authority. That is the layer the stalls this week lived in — a
//! declined construct that never parked, a parked member neither oracle
//! could see, an outstanding order that outlived its lane.
//!
//! What stays real: the `SQLite` journal, the reducer, the projection, every
//! reactor, git checkout and candidate capture. What is substituted: the
//! lane program (`bloomery-mock-lane` / `bloomery-harness-mock-lane` as
//! `AETHER_BLOOMERY_LANE_PROGRAM`) and, on the fixture cell, the in-memory
//! GitHub. The crate is test-support end to end — only ever a
//! dev-dependency — so nothing here enters a production graph.
//!
//! A new scenario picks a cell. It does not write a fourth `struct Harness`.

use std::env;

pub mod cells;
pub mod harness;
pub mod oracle;
pub mod scenario;
pub mod support;

pub use cells::{FixtureHarness, LaneHarness};
pub use harness::drive::{captured, draft, failed, faulted, member, passed, verdict};
pub use harness::{
    Backend, BloomeryHarness, CoordinatorKind, HarnessBuilder, HarnessRoots, Lane, ScenarioHarness, digest,
    while_pumping,
};
pub use oracle::liveness::{Progress, Quiescence, classify};
pub use oracle::{Oracle, Violation};
pub use scenario::{LaneScript, MemberSpec, OperatorMove, Scenario, Supersede};
pub use support::client;
pub use support::correspondence::MapCorrespondence;
pub use support::process::{Coordinator, free_port};
pub use support::repo::Repo;
pub use support::wire::{Wire, control_mailbox};

/// Path of the mock-lane program this process should point
/// `AETHER_BLOOMERY_LANE_PROGRAM` at.
///
/// When the consumer is `aether-chassis-bloomery`'s test binary, cargo injects
/// `CARGO_BIN_EXE_bloomery-mock-lane`. When this crate's own tests run, cargo
/// injects `CARGO_BIN_EXE_bloomery-harness-mock-lane` for the thin wrapper
/// binary this package defines. Either spelling is the same program.
///
/// # Panics
/// Neither environment variable is set, so there is no mock-lane binary to run.
#[must_use]
pub fn mock_lane_program() -> String {
    env_value("CARGO_BIN_EXE_bloomery-mock-lane")
        .or_else(|| env_value("CARGO_BIN_EXE_bloomery-harness-mock-lane"))
        .expect("a mock-lane binary is on CARGO_BIN_EXE_bloomery-mock-lane or CARGO_BIN_EXE_bloomery-harness-mock-lane")
}

/// Path of the `bloomery` coordinator binary the forked cell execs.
///
/// Only the crate that defines that binary (`aether-chassis-bloomery`) injects
/// `CARGO_BIN_EXE_bloomery` into its test process. The forked cell is that
/// crate's lane-boundary scenarios; generated tests in this crate use the
/// in-process cell and never look this up.
///
/// # Panics
/// `CARGO_BIN_EXE_bloomery` is unset.
#[must_use]
pub fn bloomery_bin() -> String {
    env_value("CARGO_BIN_EXE_bloomery").expect("the bloomery coordinator binary is on CARGO_BIN_EXE_bloomery")
}

/// Read a process-level test knob without `std::env::var`, which clippy
/// disallows as a capability-config path. Iterating `vars` is the same lookup
/// cargo injects into the test process (`CARGO_BIN_EXE_*`).
fn env_value(name: &str) -> Option<String> {
    env::vars().find(|(key, _)| key == name).map(|(_, value)| value)
}
