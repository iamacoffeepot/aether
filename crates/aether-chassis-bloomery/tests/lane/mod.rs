//! The lane-boundary cell of the scenario harness (#4727): a real coordinator,
//! driven through a real `git worktree add` and a real lane subprocess, with the
//! mock lane binary as the only substitution.
//!
//! This module is the lane cell's constructors and liveness tripwires. The
//! harness itself is [`crate::harness`]: one builder, three axes, named cells.
//! Reach here when the scenario must prove the coordinator makes progress
//! through its durable work loop.
//!
//! # What stays real
//!
//! Everything. The forked `bloomery` bin is the production binary, booting the
//! production chassis: the `SQLite` journal, the reducer, the projection, every
//! reactor, the outbox drain, the poll timers. The dispatch runs the real
//! `ProcessTransformRunner` — the real `git worktree add --force --detach`, the
//! real environment scrub, a real child process, its real exit status, a real
//! `evidence.json` on disk, and the real candidate capture that commits the
//! scratch worktree. Only the program at the end of the argv is substituted,
//! through `AETHER_BLOOMERY_LANE_PROGRAM`.
//!
//! That boundary is the point. The existing seam substitutes a `TransformRunner`
//! and so skips every step above; four of the six failures that stopped a live
//! run live below it.
//!
//! # Why it forks rather than boots in-process
//!
//! Two reasons, and the second is the decisive one. Boot construction is what
//! decides which store a reactor opens and which backend it mounts, and a
//! scenario that builds those itself is not testing the thing that has broken.
//! And the git the dispatch shells has no `-C`: it resolves against the
//! coordinator's *process* working directory. A forked coordinator gets its own,
//! pointed at a scratch repository — so scenarios stay isolated and still run
//! concurrently, where an in-process harness would have to serialize on a
//! process-global `chdir`.
//!
//! # The shape of a scenario
//!
//! ```ignore
//! let mut harness = LaneHarness::start(LaneScript::all_passing());
//! let bloom = harness.settle("the member resolves", |bloom| bloom.members[0].resolution.is_some());
//! ```
//!
//! [`LaneHarness::settle`] polls the projection to a budget and checks both
//! liveness invariants on every poll, so a scenario never has to ask for them
//! and cannot forget to.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its coordinator reports it by panicking")]

pub mod liveness;

pub use crate::harness::{LaneHarness, while_pumping};
