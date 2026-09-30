# ADR-0245: A Timer Is a Driver-Native Program

- **Status:** Proposed
- **Date:** 2026-09-30

Builds on [ADR-0226](0226-native-bundle-driver.md) (the bundle driver: one
`Requested` then one `Transition` or `Fault` per request, the restart pass of
decision 9, and `AwaitProcessed` in decision 11) and
[ADR-0234](0234-a-muse-turn-is-one-sampled-program.md) (a `Transient` turn is
retried by its caller, and waiting out `Retry-After` was left to this ADR).

## Context

Nothing in the bloomery can wait out a delay: a vendor's `Retry-After`, a
timeout, or a cron-style schedule. No program, reactor, or the driver has a
clock.

- **A program cannot sleep.** The driver keeps one active request per bundle
  digest (`ProgramCore::enqueue_request` and `release` in
  `crates/aether-bloomery-driver/src/runtime/programs/pipeline.rs`; ADR-0226
  Consequences: "one `Invoke` per root serializes each bundle's programs"). A
  program that waited would hold its bundle's only slot for the whole wait, so
  a retry wait in the muse bundle would block every `muse.turn` behind it.
- **Rules cannot read time.** `At` carried only `seq` and `cause`, and
  `Entry::recorded_at_millis` was documented as "A fold never reads it". A
  rule that read a live clock would decide differently on every replay.
- **Restart faults everything.** ADR-0226 decision 9 records
  `Fault { Interrupted }` for every outstanding request
  (`ProgramCore::derive_startup`), so a pending wait would need a re-arm rule
  in every consumer.
- **Barriers count every outstanding request.** `AwaitProcessed` waits until
  no request at or below its bound is outstanding
  (`ProgramCore::check_processed`), so a pending week-long wait would hold
  every barrier for a week.

The first consumer is #7265: a Muse session retrying a `Transient` turn after
`retry_after_secs`.

## Decision

1. **`clock.until` is a driver-native program.** One reserved identity lives
   in `aether-bloomery-kinds` (`program/clock.rs`):
   - the head `CLOCK: Head<OpaqueBytes> = "bloomery.clock"`;
   - the bundle identity
     `CLOCK_BUNDLE = Digest::from_bytes(*b"aether.bloomery.clock.native.v1\0")`,
     which is not a content hash;
   - the input `Until { due_millis }` (`bloomery.clock.until`) and the
     result `Fired { due_millis }` (`bloomery.clock.fired`).

   A rule or a native `Call` names it like any program: program `CLOCK`,
   name `clock.until`. The driver resolves `CLOCK` to `CLOCK_BUNDLE` before
   it consults the journal's head bindings (`program_bundle` in
   `crates/aether-bloomery-driver/src/runtime/clock/mod.rs`, called by
   `plan_intents` and `derive_requested`), so a binding recorded under that
   name is never read. The request is recorded as
   `Requested { program: ProgramRef(CLOCK_BUNDLE, "clock.until"), .. }`, and
   its outcome is a `Transition` or `Fault` caused by it, like any program's.
   `ClockUntil` in `aether-bloomery-program` is a hand-written `Program`
   (`Sampled`, `Until` to `Fired`) with no `run`, so `Ran<ClockUntil>` types
   the run for a rule's trigger. No bundle hosts it, so no clock bundle is
   ever loaded.

   This is a second dispatch path beside ADR-0226 decision 3: a request
   whose bundle is `CLOCK_BUNDLE` never enters a digest queue, is never read
   as a bundle, loaded, or invoked, and holds no slot while it waits.

2. **Arming.** `enqueue_request` sends a clock request to
   `ProgramCore::arm_clock`, which:
   1. faults `BundleUnavailable` for any name other than `clock.until`;
   2. reads the `Until` input through the artifact cache or one
      `ReadArtifact` (`ArtifactRead::Until`), faulting `InputMissing` for a
      missing input, `InputDecode` for one that is not an `Until`, and
      `BundleUnavailable` for a failed read;
   3. refuses, with `Fault { Refused }`, a due time more than
      `MAX_DUE_AHEAD_MILLIS` (7 days) after the request's own
      `recorded_at_millis`, which `Request::recorded_at_millis` now carries;
   4. pushes `(due_millis, seq)` onto the core's min-heap (`Timers`).

   Pending timers share the driver's bound on outstanding requests, the
   journal fold, with no separate cap. A timer costs two small index entries.

3. **Firing is a tick.** The core stays sans-io and never reads a clock.
   While any timer is armed it keeps one `Command::ArmTick` outstanding. The
   shell performs it as a worker that sleeps one tick period
   (`ctx.dispatch_blocking_resumed_with`), and the `#[handler(task)]`
   completion reads the injected clock and calls
   `ProgramCore::tick(now_millis)`. The tick pops every timer due by then and
   queues one `PendingWrite::Fired`, appended as one `AppendRecords` per
   `EVENTS_PAGE` timers: a `Transition { program, input, result }` caused by
   each request, with each distinct `Fired` staged once. Callers waiting on a
   native `Call` are answered as for any outcome. Because each tick compares
   the clock with the heap, a forward jump or a resume from suspend fires
   every overdue timer on the next tick.

   The tick's wait holds no settlement chain. An armed timer waits on time,
   not on the work of whichever chain armed it, so a chain that sets a timer
   settles without waiting it out. A `Call` of `clock.until` still waits for
   its answer on its own held reply.

   The period is the ADR-0090 knob `clock_tick_millis` on `BloomeryConfig`
   (default 1000; `0` is refused), lowered into `DriverParams::tick`. The
   clock is `DriverParams::clock: Arc<dyn Clock + Send + Sync>`, the same
   instance the chassis opens the journal with. The contract: a timer's
   `Transition` is never recorded before its due time in journal time, it is
   recorded within about one tick after it while the engine is healthy, and
   there is no upper bound under load.

4. **Journal time is monotone.**
   - `Journal::append` stamps `recorded_at_millis` as
     `max(last_recorded, wall_now, batch.not_before_millis)`, and
     `last_recorded` is seeded from `MAX(recorded_at_millis)` when the
     journal opens. Journal time never goes backwards.
   - `AppendRecords::not_before(millis)` (and `Batch::not_before`) sets the
     floor; every other caller keeps `0`. A fired batch sets it to its latest
     due time, so "recorded at or after the due time" holds even when wall
     time steps backward between the tick and the append.
   - `Journal::open_with_clock` refuses to open with
     `JournalError::ClockBehind { wall_millis, last_recorded_millis }` when
     the clock reads more than one day before the latest entry. An append
     whose clock reads more than one second behind the latest entry logs a
     warning and is stamped at recorded time.

5. **Rules read only recorded time.** `At` gains `recorded_at_millis`,
   copied from the entry. Folds, guards, and rules read time only as
   recorded and never read a live clock. The authoring helpers are
   `wait(at, millis)`, which builds
   `Until { due_millis: at.recorded_at_millis + millis }`, and
   `wait_spread(at, millis, spread)`, which adds a deterministic offset in
   `0..spread` mixed from `at.seq` (splitmix64), so a burst of rules
   triggered by neighbouring entries spreads out.

6. **Restart re-arms timers.** A timer has no side effects. `derive_startup`
   leaves outstanding clock requests out of its `Fault { Interrupted }`
   batch, a journal whose only outstanding requests are clock requests
   needs no startup write, and recovery re-arms each one through
   `arm_clock` (`finish_recovery`). A due time that passed while the engine
   was down fires on the first tick. Every other outstanding request keeps
   ADR-0226 decision 9's `Interrupted`.

7. **Armed timers are not outstanding work.** `check_processed` does not
   count a request armed on the heap. A request still reading its input, or
   fired but not yet read back, counts as before.

8. **No cancel: a timeout is a race.** A rule requests both the work and a
   `clock.until`. Whichever `Transition` arrives first advances the rule's
   view, and the loser's outcome is ignored by its guard. **Cron** is a rule
   on `Ran<ClockUntil>` that does its work and requests the next due time,
   computed from recorded time.

## Consequences

- A rule can wait, time out, or run on a schedule without holding any
  bundle slot, and thousands of pending timers cost kilobytes and one tick
  per period.
- The driver has a second dispatch path. Every head-resolution site goes
  through `program_bundle`, and any future site must too, or it would treat
  `bloomery.clock` as an ordinary binding.
- The driver reads time once per tick while a timer is armed and not at
  all otherwise. A tick costs one short-lived worker thread.
- Journal time is monotone for every entry, not only timers. A host whose
  clock is more than a day behind its journal refuses to open the journal
  until the clock is fixed, where before it silently recorded the old time.
- A fired timer's `Transition` may be stamped later than wall time when the
  clock stepped backward. The stamp says when the journal recorded it, which
  is the only time rules read.
- Artifact rows written through the streaming store are still stamped from
  `SystemClock` directly, outside the monotone rule. No rule reads them.
- There is no status reply on the driver, so pending and fired timer counts
  are not exposed.
- Follow-up, split out: `clock.every { period_millis }`, one shared chain
  per distinct period aligned to `k · period` in journal time, with a
  driver-originated `Requested` per boundary under a new native origin and a
  restart rule of its own.

## Alternatives considered

- **`clock.until` as a bundle program over a native Timer `ProgramApi`
  provider, like `Http`.** The run would hold its digest's single active slot
  for the whole wait, and two timers in one bundle would fire in FIFO order,
  not due order.
- **A clock bundle hosting `clock.until`.** It still pays the digest slot and
  needs a load. The reserved identity needs neither.
- **Fault clock requests `Interrupted` on restart.** Every consumer would
  carry a re-arm rule, and the restart would record a fault for nothing.
- **A sleep per timer, chunked re-checks, or one OS timer per due time.** One
  periodic tick fires every due timer in one batch and handles jumps and
  suspend the same way.
- **Accept a sub-second window instead of a `not_before_millis` floor.** A
  backward wall step between the tick and the append could then stamp a
  `Transition` before its due time.
- **Count armed timers as outstanding for `AwaitProcessed`.** A barrier would
  wait out the longest pending timer, and a cron chain would never settle.
- **Hold the arming chain open across the tick's wait.** A chain that set a
  timer would not settle until it fired, and a cron chain never would.
- **Cancel entries.** A cancel costs one record, as the loser's firing does,
  and adds a record kind, a fold rule, and a cancel-versus-fire race.
