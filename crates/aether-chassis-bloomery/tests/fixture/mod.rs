//! The in-process fixture cell of the scenario harness (#4711): a real
//! coordinator chassis, booted in the test process against temporary stores and
//! an in-memory GitHub, driven one explicit tick at a time.
//!
//! This module is the fixture cell's constructors and scripted-verdict helpers.
//! The harness itself is [`crate::harness`]: one builder, three axes, named
//! cells. Reach here when the scenario is a reactor-to-reactor handoff.
//!
//! # What stays real
//!
//! Every outbox row's producer and consumer. The reducer decides, the control
//! core commits its decisions into the store's outbox topics, and the
//! boot-constructed reactors drain those exact rows — no scenario ever places
//! one. That is the whole point: the reactor-to-reactor handoff is the thing
//! this tier tests, and a fixture that enqueued the row it claims to prove would
//! test the enqueue and nothing else.
//!
//! # What is substituted
//!
//! Two things, not one.
//!
//! The **verdict** a model would have produced arrives through
//! [`ScriptedEvidence`](aether_chassis_bloomery::bloomery::ScriptedEvidence),
//! which the executor reactor admits through its own outstanding-order registry
//! — so a scenario can only answer an order the coordinator really dispatched,
//! bound to the digest that order really displayed.
//!
//! The **candidate push** is substituted too, and less visibly. Production's
//! pull path ends by resolving an admitted capture's checkout through
//! correspondence and force-pushing that commit to the bloom's candidate ref
//! (ADR-0152); the scripted admit path omits that step, and
//! [`seed_capture`](FixtureHarness::seed_capture) plants the same ref itself
//! through the same `candidate_ref_name` helper. The omission is forced — the
//! production pusher shells a real `git push --force origin` — but the cost is
//! real: a wrong ref name, a dropped push, or a mis-resolved correspondence is
//! invisible here, because the fold reads a ref this harness wrote. That step
//! belongs to the lane-boundary tier, which runs a real pusher.
//!
//! # Warning: do not seed a completed run here
//!
//! The reason no scenario has ever run that production pusher is narrower than
//! it looks. `on_dispatch_tick` calls `pull_and_admit` unconditionally, dozens
//! of times per scenario, and `pull_and_admit` ends in the push. What keeps the
//! push loop empty is that `FakeGithub::dispatch_workflow` records a dispatch
//! and never a run, so `find_run` answers `None` and the intake cycle matches
//! nothing.
//!
//! One `seed_run(nonce, Completed, Success)` plus `seed_run_artifacts` — the
//! obvious next step for anyone extending this harness toward the real pull
//! path — removes that. The correspondence this harness already seeds then
//! resolves the capture's checkout to a real commit, and the push loop reaches
//! the reactor's pusher with it.
//!
//! What that pusher now is has changed (#4842): the boot seam selects on build
//! shape, so any `testing`-featured binary — every binary `cargo test` forks —
//! carries the refusing arm and cannot shell `git push` whatever backend it
//! names. A scenario extended this way therefore gets a logged refusal rather
//! than a force-push to the developer's own `origin`.
//!
//! That is a backstop, not the design. A refusal is still a scenario failing
//! for a reason that has nothing to do with what it set out to prove, so give
//! the reactor a recording pusher through `ExecutorReactorState::with_pusher`
//! before writing that scenario, and read the pushes it recorded.
//!
//! # Why it boots in-process rather than forking
//!
//! Boot construction is what decides which stores a reactor opens, and the two
//! roots it opens are named by *different* configs: the executor reactor opens
//! its journal and its artifacts handle from `CoordinatorConfig`, while the
//! store and artifacts capabilities open theirs from `StoreConfig` and
//! `ArtifactsConfig`. Pointing each pair at one temporary root is what makes a
//! reactor that resolved a different one — a platform data dir, say — fail here
//! instead of filing real records where nothing reads them (#4705). Owning the
//! roots is also what lets a scenario read them directly, which a forked
//! coordinator's wire surface does not expose.
//!
//! # One scenario per test binary
//!
//! `GithubConnectionConfig::shared_fixture` is a process-global `OnceLock`:
//! first caller wins and it never resets. Every consumer inside one coordinator
//! wants exactly that — the correspondence store, the source shell, the
//! projection shell and the executor shell all have to see one repository — but
//! two scenarios in one process would share a repository and a mainline. So each
//! behavior gets its own binary, and this module is compiled into each.
//!
//! Each of them declares it `pub mod fixture;`, which is load-bearing rather
//! than decorative: the harness surface is one thing, and the scenarios that
//! consume it are three. A `study_index_row` reachable only from the scenario
//! that measures an attempt cost is unreachable in the other two binaries, and
//! a private module would have each of them report it as dead — a signal about
//! how this tier is split, not about the code. Declaring the module public makes
//! its surface reachable in every binary, which is what it is: the item is part
//! of the harness whether or not the binary compiling it happens to call it.
//!
//! # The cadence is off, not slow
//!
//! Every reactor's timer runs at `poll_interval_secs.max(1)`, so there is no
//! value that means "never" — `0` polls fastest. The fixture cell's default is a
//! day, which inside a scenario is the same thing as never, and progress comes
//! from the explicit ticks. Each reactor's `wire` still fires one boot tick;
//! at boot there is nothing enqueued for it to find.
//!
//! # The shape of a scenario
//!
//! ```ignore
//! let mut harness = FixtureHarness::start("my-scenario");
//! let bloom = harness.seal_member("wp", digest(0x51));
//!
//! let construct = harness.await_order();
//! let candidate = harness.seed_capture(bloom, "wp", digest(0xC1), digest(0xC2));
//! harness.upload_admitted(&captured(&construct, candidate));
//!
//! let verify = harness.await_order();
//! harness.upload_admitted(&passed(&verify));
//! harness.land_the_fold(bloom);
//! ```

pub use crate::harness::FixtureHarness;
#[doc(inline)]
pub use crate::harness::digest;
pub use crate::harness::drive::{captured, draft, member, passed, verdict};
