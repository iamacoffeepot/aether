# Testing

A test earns its place when you can name the bug it would catch and that bug is
plausible — a real branch, boundary, invariant, or wire contract a future edit
could break without also breaking the test. That is the whole bar. Before you
write a test, finish this sentence: *"this fails if someone ___."* If the only
honest ending is "edits the test," the test is junk and CI is poorer for it.

The sharpest form of that question is **what logic owned by this crate does the test
exercise?** Junk tests routinely pass the first question with a plausible story — "it
pins the wire name," "it round-trips the kind," "it proves the kind is registered" —
while exercising no logic this crate wrote. They run a `#[derive]` macro, the shared
codec, or the inventory registration, all owned and tested once elsewhere, and confirm
the value came back. If the only honest answer is "none of ours — it restates a
declaration or re-runs machinery another crate already tests," the test is junk no
matter how much ceremony surrounds it. Field-by-field assertions, a non-trivial value
under test, and a confident doc comment are not evidence of load; they are the
camouflage junk hides behind.

Junk tests are not free. Each one spends compile time, run time, and reviewer
attention, and every false sense of coverage it adds is a place a real regression
can slip through unnoticed. A small suite of load-bearing tests beats a large suite
padded with tests that pass no matter what the code does.

## What does not clear the bar

These shapes recur. None of them can fail for a reason you care about, so none of
them belong in the suite:

- **Mirror tests** restate the source as an assertion. `assert_eq!(Foo::default().x, 0)`
  sitting next to `x: 0` in the `Default` impl breaks only when someone edits both
  halves together, which is to say never on its own. The common disguise is the
  derived constant: `assert_eq!(NoteOn::NAME, "aether.audio.note_on")` reads like it
  guards the wire name, but `NAME` *is* the `#[kind(name = "…")]` literal — the
  assertion's expected value is the same string retyped, with no independent source of
  truth. A rename edits the attribute, and the test sitting beside it is updated in the
  same motion. Every real consumer routes on `NoteOn::NAME` or its hash, so they track
  the rename for free; the literal in the test is the one copy nobody downstream uses.
- **Round-tripping a derive-only type.** `decode(encode(x)) == x` over a type whose
  `Serialize` / `Deserialize` / `Schema` are all `#[derive]`d tests nothing this crate
  wrote. The roundtrip is **symmetric**: encode and decode are generated from the same
  definition, so any change to one changes the other in lockstep and the test still
  passes. It can fail only if the two *disagree* — which for a derived type means the
  derive macro is broken, and that is tested where the macro lives. Building an
  elaborate value and asserting each field survives does not change this; it confirms
  the shared codec is an identity function over your struct, which is the codec's
  invariant, tested once in `aether-data`. A roundtrip earns its place only when the
  type has hand-written ser/de, or an invariant the roundtrip actually exercises (a
  clamp, a normalization, a rejected input) — not when it is plain derives over plain
  fields.
- **Testing code you do not own.** The unit under test must be logic we wrote. A
  test that exercises a dependency's behavior catches a bug only the dependency's
  authors can fix, and we keep it green by never upgrading. This covers the standard
  library and the compiler (pushing three items and asserting `len() == 3`, checking
  that `#[derive(Clone)]` clones), serde and every other third-party crate (does
  `wgpu` clear the surface, does `tokio` schedule the task, does `fontdue` rasterize),
  and any generated code whose generator already has its own tests. The codec is the
  trap here: it is ours, but it is owned *once*, in `aether-data`. Testing it means
  testing it there — re-running it from a consumer crate on a consumer's struct tests
  the consumer's `#[derive]`s and the shared codec, neither of which is that crate's
  logic. When a test fails, the fix should land in the crate the test lives in — if it
  would land in someone else's, the test was never yours to write.
- **Re-testing shared engine machinery from a consumer.** Some logic we own is owned
  *once* and already tested where it lives: config resolution through
  `#[derive(Config)]` (argv > env > default, whether an env var is set, how the string
  parses), mail routing, settlement, id and lineage hashing, and everything the `Kind`
  / `Schema` derives emit. A capability test that asserts its own knob picks up
  `AETHER_FOO`, or that a missing variable falls back to the default, is testing the
  `Config` derive rather than the capability. The same trap wears other masks:
  - *Derive-emitted registration.* Asserting a kind appears in `descriptors::all()`
    guards nothing — `#[derive(Kind)]` emits the `inventory::submit!`, so the entry is
    present by the fact the type derives `Kind` and the crate is linked. There is no
    manual registration to forget; a missing entry means the derive is broken (tested
    elsewhere) or the type was deleted (a compile error, not a test failure).
  - *Schema-shape assertions.* `assert!(matches!(Role::SCHEMA, SchemaType::Enum))`
    restates the `enum` keyword through the derive. The derive maps `enum` → `Enum`
    and `struct` → `Struct` mechanically; the assertion adds nothing the declaration
    does not already say.

  Test what the capability *does* with these — the value it computes, the mail it sends,
  the input it rejects — and let the machinery's own suite cover that the derive, the
  codec, and the registry work.
- **Mock theater.** A test that stands up so many fakes it only ever exercises the
  fakes, asserting that a mock returns what the test told it to return. It verifies
  the setup, not the system.
- **No real assertion.** Calling the function and never checking the result;
  asserting only that it "didn't panic"; or checking the output against a value the
  test recomputes the same way the code does. An assertion needs a known-good oracle,
  not a second copy of the implementation.
- **Vacuous bodies.** `assert!(true)`, an empty test, a loop that runs zero times, an
  early `return` ahead of the assertion, or a guard that skips on every machine that
  will ever run it.
- **Bulk duplication.** Ten near-identical cases driving one branch with different
  literals. One table-driven case carries the same signal; the other nine are noise.
- **Coverage chasing.** A test written to turn a line green rather than because the
  behavior matters — a trivial getter, a `Display` impl with no logic, an
  exhaustiveness arm that can never be reached.

## The tripwire that looks like junk

A deliberately boring test can be load-bearing. Pinning the `MailboxId` lineage hash,
a wire format's byte layout, or a `KindId`'s numeric value reads like a mirror test —
a flat assertion against a fixed value. The difference is what sits on each side. A
tripwire pins a **computed** value — a hash, a serialized byte layout, a derived id —
against an independent constant, so it fails when the *logic that produces the value*
drifts even though the declaration that named it did not. That is a real contract:
downstream code depends on the computed value, the value can change invisibly, and the
test makes the change loud.

This is exactly what a derived-constant mirror is not. `assert_eq!(NoteOn::NAME,
"aether.audio.note_on")` has the declaration on one side and a copy of it on the other
— nothing is computed, nothing can drift on its own. If you want to guard a kind's wire
identity, pin the thing consumers actually route on and that *is* computed from the
name: `assert_eq!(NoteOn::ID, KindId(0x…))`. That fails if the hashing changes or the
name changes, both of which move the id without touching any line a reader would notice.
Pinning the name string against its own literal guards none of that.

Mark a genuine tripwire so the next reader (and the next sweep) can tell it from junk.
A one-line comment naming the invariant and why it is pinned is enough:

```rust
// Tripwire: this byte layout is the wire contract with the hub. A change here
// breaks every connected engine — if this assertion fails, update the protocol
// version, do not just re-bless the bytes.
assert_eq!(frame.as_bytes(), EXPECTED_WIRE_BYTES);
```

The comment is necessary but not sufficient: a comment over a value that cannot drift
on its own is a mirror with a story told over the top, and the sweep treats it as junk.
The contract is real only when the pinned value is computed.

## Where the test goes

Once a test clears the bar, the harness follows from what it checks. Engine-internal
correctness goes to **SubstrateHarness** (`aether-harness-substrate`) with a concrete
assertion (`captured`, `reply`, `count_observed`); visual reductions and failure
artifacts go through **SubstrateHarness Capture** (`aether-harness-substrate-capture`).
Behavior over the wire — recipient-name resolution, fleet lifecycle, the RPC boundary — goes to
**FleetHarness** (the `aether-harness-fleet` crate). FleetHarness is
headless, so any rendered-output assertion has to use SubstrateHarness plus its capture
extension, and any
externally-addressable-over-the-wire assertion has to be FleetHarness.

Bloomery coordinator behavior has a third boundary: use
**LaneHarness** for a scenario that must prove the coordinator makes progress
through its durable work loop, rather than merely that a reducer or runner
returns an expected value. It forks the production coordinator and drives its
real local-lane boundary while replacing only the expensive transform program
and GitHub service. That makes it the right tier for dispatch, evidence, retry,
wedge, aggregate, and liveness contracts; it is not a model-quality evaluation
or proof that the real transform program, live credentials, or GitHub transport
work. See [SubstrateHarness, FleetHarness, and LaneHarness](testing/substrateharness-and-fleetharness.md#laneharness-topology).

A LaneHarness scenario about a lane that hangs needs the sealed dispatch deadline
(ADR-0177), not a scenario-side timeout. Every dispatched order carries an
absolute deadline in Unix milliseconds, computed once when the coordinator
durably records the order from the `wall_clock_secs` its bloom's stage catalog
sealed. `LaneHarness::start_with_wall_clock` authors a catalog binding every
stage at a few seconds and seals its address into the bloom, which is the only
way in: the limit is deliberately sealed rather than ambient, so two blooms
sealing the same catalog terminate identically and no coordinator-side override
exists for a scenario to reach for. Past that deadline the run is cancelled, so a
child process the coordinator tracks — and the scratch worktree checked out
behind it — are reclaimed whatever stage was dispatched. For a member stage or
`AggregateVerify` the attempt is then recorded as an ordinary failure, so retry
and wedge assertions read exactly as they do for a lane that failed outright.

`AggregateReview` is the carve-out: its expiry reclaims the run and reports the
deferral, but records no verdict, and the order stays outstanding rather than
being consumed. A critic that never answered produced no judgement of the fold,
and the verdict that states exactly that — ADR-0176's `ExecutorFault` — arrives
with issue #4738; the nearest available verdict would charge every member a
repair lap for a critic that never ran. Until then a scenario about an
aggregate-review lane that hangs can assert the cancellation, and only that: no
timeout record, no admitted attempt, and no movement of the retry or wedge
lifecycle follows from the deadline passing.

A restart is part of that contract rather than an escape from it, on the
accounting side. The deadline is persisted beside the order, so a coordinator
that stops and reopens reads back the same number: a scenario can restart
mid-flight and still assert the original expiry, and one that expected a restart
to renew the allowance is asserting the bug.

Reclamation survives that restart in part, and a scenario has to be exact about
which part. A boot reconciles the two things that outlive the process: the orders
the store still holds outstanding, and the scratch root at
`.bloomery/local-worktrees/`. An outstanding order whose directories survived is
re-adopted and routed back to the local arm, so its expiry cancel reaches the arm
that holds the run and reclaims the `<nonce>` checkout along with its `git
worktree` registration — and a directory belonging to no outstanding order is
swept in the same pass. Re-adoption also means an attempt that finished while the
coordinator was down still admits from the `evidence.json` its run left behind,
rather than riding to its deadline.

What does not come back is the child process. A coordinator holds no handle on
one it did not spawn and records no pid, so cancelling a re-adopted run warns
that the lane may still be running and removes the ground under it rather than
killing it. Assert the accounting and the checkout reclaim across a restart;
assert termination of the child only within one process's lifetime.

The advisory `stale_warn_after_secs` sweep is unrelated to both — it warns about
an unresolved handle and terminates nothing, so no scenario should wait on it.

Beside it sits the **fixture harness** (`crates/aether-chassis-bloomery/tests/fixture/`),
which boots the same production chassis inside the test process and steps it one
explicit reactor tick at a time. Its reason for existing is the handoff *between*
reactors: each reactor's own unit tests hand-place the outbox row its upstream would
have produced, so a producer that emits a payload its consumer cannot act on passes
both sides and stalls only in production. Here every row comes from the real reducer's
decisions committed by the real control core, and the boot-constructed reactor that
owns the topic drains it. Nothing in a scenario enqueues a topic.

Under both sits the oldest of the three, the **cross-process tests**
(`crates/aether-chassis-bloomery/tests/`: `rest_api.rs`, `control_loop.rs`,
`recovery.rs`). Each boots the real `bloomery` binary over a socket and drives it the
way an operator or a crash would — `curl` against the HTTP ingress, typed mail over
RPC, `kill -9` and restart against the same database file. They are the only tier that
kills and restarts its coordinator, which is exactly what they are for.

| | LaneHarness | fixture harness | cross-process |
| --- | --- | --- | --- |
| Coordinator | forked `bloomery` process | booted in the test process | forked `bloomery` process, killable |
| Substituted | the lane program at the end of the argv | the model's verdict, and the ADR-0152 candidate push | nothing below the wire; the bloom's own evidence is synthetic |
| Progress comes from | the coordinator's own poll cadence | explicit `DispatchTick` / `IntegrateTick` / `LandTick` calls | the coordinator's own poll cadence |
| Reads | the projection and the journal over the wire | those, plus the store and artifact roots on disk | HTTP responses, typed mail replies, and the database file across a restart |
| Proves | the whole dispatch below the lane: `git worktree add`, the child process, its `evidence.json`, the candidate capture | the reactor-to-reactor handoff, and that a boot-constructed reactor resolved the roots it was configured with | that state survives a process boundary — journal replay, outbox republish, a losing concurrent seal, the REST surface an operator actually types at |
| Cannot prove | that a stage transition produced the next reactor's input, since a lane failure and a missing handoff both read as a stall | anything about the lane subprocess, which it never spawns; the candidate push, which it substitutes; or the coordinator code an uploaded verdict would have travelled through. `ExecutorShell::inspect` does run here, on every dispatch tick, but the in-memory GitHub records a dispatch and never a run, so it only ever answers `Unknown`; its completion branch therefore never opens, and `ExecutorShell::stream_evidence` and the `NameEvidenceClaims::claim_for` decode paired with `attempt_artifact_name` that produces `verdict` and `failed_verifiers` never run — `admit_scripted` builds an `UploadedEvidence` directly instead | that a bloom advances, since nothing supplies a lane verdict |

**Where a new test goes.** A crash, a replay, a restart, or a race between two
concurrently running loops has to be cross-process: neither of the other two tiers can
kill and restart its coordinator, and the fixture tier steps its reactors one at a time
by construction, which removes the interleaving a race needs. Anything about the lane
subprocess or the candidate push belongs to LaneHarness, which runs both for real.
Everything else about how the coordinator's own parts fit together — a reactor drains
what its upstream projected, a boot-constructed reactor opened the configured root —
belongs to the fixture tier, which is the fastest of the three and should carry the
bulk.

Two constraints follow from the fixture tier's in-process boot, and both are structural
rather than incidental. The in-memory GitHub double is a process-global `OnceLock`, so
each behavior gets its **own test binary** — two scenarios in one process would share a
repository and a mainline. And no reactor's poll interval can be set to "never"
(`poll_interval_secs.max(1)`, where `0` polls fastest), so a scenario sets a cadence far
enough out that the timer never fires inside it and drives every step by hand.

For overlay rendering, split structural and raster proof deliberately. Assert exact
rectangle geometry, clips, texture coordinates, tint, texture identity, projection
space, and submission order through `SubstrateHarness::committed_overlay_snapshot`; then use
`CaptureFrame` reductions for the smaller set of outcomes that need end-to-end proof
through projection, blending, rasterization, and GPU readback. The typed snapshot
contains only batches accepted into the recorded draw plan, so missing textures,
invalid/empty clips, and an over-budget overlay pass cannot masquerade as rendered
work. It localizes a malformed submission, while the rendered capture proves the
pipeline actually produced the intended pixels.

A test that drives neither harness and exercises none of our own pure logic is the
case to look at hardest, because there may be no engine behavior under it at all. Our
pure logic — the codec, `aether-math`, schema encode/decode, id and lineage hashing —
is load-bearing to test *in the crate that owns it* (`aether-data`, `aether-math`).
Re-running it from a consumer crate, on a consumer's derived type, is the junk case
above, not a second copy worth keeping.

Within a SubstrateHarness visual test, reach for the narrowest oracle first. A concrete typed
observation (`reply::<R>`, `count_observed`) beats a pixel check whenever the mail
already carries the answer. When the behavior is genuinely visual, the
`aether_harness_substrate_capture::visual` frame reductions (`not_all_black`, `differs_from_background`,
`coverage`, `centroid`, `bounding_box`) turn a captured PNG into a scalar or coordinate
assertion — pin a band, not an exact pixel, since GPU / anti-aliasing nondeterminism
makes an exact golden image the wrong primary oracle.

## Diagnosing a failing visual assertion

A frame reduction that fails leaves only its scalar diagnostic in the test log — the
captured pixels it was scored against are gone by the time you read it. Arm
`aether_harness_substrate_capture::ArtifactGuard` around the capture and its checks for a widget-
heavy scenario where that scalar isn't enough to see what actually rendered:

```rust
let mut guard = ArtifactGuard::arm(
    "widget_panel_layout",
    png.clone(),
    checks.clone(),
    verdict.results.clone(),
);
// ... assertions on `verdict` that may panic ...
```

The guard is a plain `Drop` type. A passing test leaves it untouched and it writes
nothing; an unwinding panic through its scope best-effort writes
`target/substrate-harness-artifacts/<id>/actual.png`, `measurements.json`, and one
`mask_N.png` per requested check — each mask rendered from the exact
region/background/tolerance partition that scored it, so the artifact can never show a
different verdict than the one that failed. A test that detects failure through a
`Result` return rather than a panic calls `ArtifactGuard::persist` explicitly before
returning `Err`. Attach an already-loaded same-size reference PNG with
`.with_reference_png(..)` to also get `reference.png` and a pixel-wise
`difference.png` — diagnostics on top of the region/mask read, not a second pass/fail
oracle; the frame reductions above stay what decides pass or fail. CI uploads each
failed shard's `target/substrate-harness-artifacts/**` tree for download; a green run uploads
nothing.

`ArtifactGuard` owns visual bytes. For a declarative harness sequence that fails
before a capture or reply assertion, use `SubstrateHarness::execute_with_diagnostics`
instead of making ordinary `execute` a filesystem writer. It retains a failure-only,
bounded JSON record under `target/substrate-harness-artifacts/execution/<id>/` while
returning the original typed `ExecutionError`; see the harness guide for its exact
fields and the visual/non-visual split.
