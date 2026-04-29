# ADR-0063: Fail Fast on Abnormal Component Lifecycle

- **Status:** Proposed
- **Date:** 2026-04-28

## Context

ADR-0038 made dispatch actor-per-component: each loaded component owns
an OS thread that pulls mail from an mpsc inbox and calls
`Component::deliver`. Issue #321 then hardened that path against
abnormal exits: a wasm trap or host-side Rust panic during `deliver` is
caught, the dispatcher entry transitions to `STATE_DEAD`, and a
`aether.observation.component_died` broadcast goes out so external
observers (Claude sessions, monitor components) don't have to poll.
ADR-0023's log capture mirrors every `tracing` event into a flushable
ring so `engine_logs` surfaces the trap message after the fact.

In steady-state observation, this works: post-mortem you can read the
trap, see which mailbox died, see what kind it was processing. The
diagnostic-after-the-fact path is solid.

What is *not* solid is the **real-time** signal during failure. Two
gaps surface during development:

1. **`drain_all` is unbounded.** The desktop chassis's `RedrawRequested`
   handler calls `queue.drain_all()` on the main thread every frame —
   the per-frame barrier that lets tick-emitted mail (camera updates,
   render-sink pushes) be observed by `gpu.render` in the same frame.
   `drain_all` waits on every component's pending counter to reach
   zero, with no timeout. A wasmtime trap from
   `alloc::raw_vec::capacity_overflow` takes 3–10 seconds to surface
   (the wasm allocator retries growth, the panic handler formats a
   stack, the trap walks back through wasmtime). For that window the
   macOS event loop on the main thread cannot service other
   `WindowEvent`s. Window unresponsive, `capture_frame` blocked, no
   signal at all about what's happening.
2. **Drain is status-blind.** When the trap finally surfaces,
   `kill_actor` flips `STATE_DEAD`, decrements pending, and exits the
   dispatcher loop. `drain` returns `()` — same return type as a clean
   delivery. The chassis at the barrier interface cannot distinguish
   "everything quiesced cleanly" from "a component died and we're
   continuing past its corpse." The `STATE_DEAD` flip and the
   `component_died` broadcast happen, but the synchronous caller that
   was *waiting on this exact event* has no structured channel to it.

The combination produces the operator experience: the application
freezes for several seconds with no signal, then comes back as if
nothing happened, with one error line buried in `engine_logs`. The
ambiguity ("is this loading, or did it stop working?") is the
load-bearing failure — the diagnostic infrastructure is in place but
the time-to-signal is too long for development.

There is also no **failover design** that would tell a substrate what
to *do* when a component dies. Soft recovery is not just "don't exit
the process" — it is a deliberate decision about what should happen
next: auto-restart the dead component from its last-loaded wasm,
notify a monitor component, cascade-die dependents, surface the death
to the operator and wait for instruction, or some mix of those. None
of those policies has been chosen. Until one is, "keep the substrate
alive after a death" is undefined behavior dressed up as resilience.

The wire-level pieces that would *enable* a chosen policy are
short-distance fixes once the design exists:

- `kill_actor` flips `STATE_DEAD` on the dispatcher entry but never
  calls `Registry::drop_mailbox`, so the registry name slot stays
  `MailboxEntry::Component` (live). A subsequent `try_register_component`
  with the same name returns `NameConflict`. The fix is a one-line
  call site in `kill_actor` plus tests; it just hasn't been written
  because nothing exercised the soft path.
- `replace_component` over MCP rounds mailbox ids through JSON's
  2^53 safe range. The fix is moving id wire-encoding off raw JSON
  numbers (string-typed opaque ids — covered in the follow-on ADR
  for prefixed-string id encoding). Modest implementation surface;
  decoupled from this ADR.
- The hub's MCP harness already uses `terminate_substrate` +
  `spawn_substrate` as the dev-loop iteration primitive; respawn-per-
  iteration is the existing daily driver, so the cost of an abort
  policy is near-zero in current usage.

So the soft path preserves optionality the current workflow doesn't
use, gated on a failover design that doesn't yet exist, at the cost
of a multi-second freeze every time something goes wrong.

## Decision

The substrate **fails fast** on any abnormal component lifecycle event:

1. **A component traps or its dispatcher panics during `deliver`.**
   `STATE_DEAD` flips, the existing `component_died` broadcast goes
   out, and the substrate then exits the process with a non-zero code
   after flushing its log ring.
2. **A drain wait exceeds a budget** (`Duration::from_secs(5)` in v1).
   The dispatcher is wedged in a way wasmtime hasn't surfaced — host
   panic loop, deadlock in a host fn, slow trap unwinding. The
   substrate logs `dispatcher wedged: mailbox=… waited=…`, emits a
   structured `substrate_dying` broadcast, and exits.

Both paths run through a single `fatal_abort(reason)` function that
synchronously flushes `log_capture`'s ring, sends the final broadcast,
closes the engine TCP cleanly, and calls `std::process::exit(2)`. The
hub treats the disconnect as "child crashed" and is free to respawn on
the next operator action.

The mechanism that lets the chassis detect both events is **drain
bubbling**: `ComponentEntry::drain` and `scheduler::drain_all` return a
structured outcome (`DrainOutcome` / `DrainSummary`) instead of `()`.
The dispatcher writes the death reason into a per-entry slot before
exiting; `drain`'s condvar wake reads that slot and returns
`Died(DrainDeath { mailbox, name, last_kind, reason })`. A `wait_timeout`
expiry returns `Wedged { waited }`. The chassis matches on the summary
and calls `fatal_abort` on anything other than `Quiesced`. This is an
implementation detail of the policy, not a separate decision — but
it's the channel that makes the policy expressible.

This ADR explicitly **defers soft recovery** — i.e. "mark the component
dead, keep the substrate alive, let the operator reload" — to a future
ADR. The load-bearing question that ADR has to answer is the failover
design: *what should happen when a component dies?* Auto-restart from
the last-loaded wasm, notify a monitor component, cascade-die
dependents, hold and wait for operator instruction, or something else.
Until that policy is chosen, soft recovery has no defined behavior to
fall back to.

The two wire-level enablers (`Registry::drop_mailbox` on death,
precision-safe MCP id encoding via the prefixed-string ADR) are
short-distance fixes that the failover ADR will pull in as
dependencies; they are not what this ADR is gated on.

This ADR builds on ADR-0038 (actor-per-component dispatch). ADR-0038
gave each component its own dispatcher thread but did not specify
chassis-level behavior when a dispatcher dies abnormally. ADR-0063
fills that gap.

## Consequences

- **Time-to-signal collapses from seconds to the drain budget.** A
  trapping component triggers a logged abort within at most 5 seconds
  in v1 (the drain budget); a clean trap that surfaces faster aborts
  proportionally faster. The frozen-window-with-no-signal failure mode
  is gone.
- **Every component death costs the whole substrate.** A crash in any
  component takes the substrate's other components with it. Acceptable
  in v1 because the daily workflow runs one component at a time and
  iterates by respawn anyway; becomes a real cost the moment
  multi-component compositions land — at which point a soft-recovery
  ADR is the trigger to soften this policy.
- **`drain_all` gains a side effect: it can decide the substrate
  should die.** The chassis no longer treats drain as a pure barrier.
  This is a one-way change: code paths that call `drain_all` need to
  match on the summary and route abnormal outcomes through
  `fatal_abort`. The desktop and headless chassis are the only callers
  in v1.
- **`log_capture` grows a synchronous `flush_now`.** The current flush
  loop is a background thread on a 250 ms timer / 100-entry trigger;
  `fatal_abort` cannot rely on it because the process is about to
  exit. `flush_now` drains the ring on the calling thread before
  exit so the abort log lands in `engine_logs`.
- **MCP semantics are unchanged.** From the operator's view, the
  substrate disconnects, the hub flags the engine offline, and a
  subsequent `spawn_substrate` produces a fresh process. The
  `component_died` and `substrate_dying` broadcasts surface the cause.
- **Future soft-recovery work has a clear shape.** The load-bearing
  decision is the failover design: when a component dies, what does
  the substrate *do*? Auto-restart, notify a monitor, cascade-die,
  hold-and-wait. Once that policy is chosen, the supporting wire
  changes — registry name-slot release on death, precision-safe MCP
  id encoding (covered in the follow-on prefixed-string ADR) — are
  short-distance implementation work, not architectural blockers.
  ADR-0063 is the superseded-by target for that future ADR.

## Alternatives considered

- **Wasmtime epoch deadlines + soft path.** Configure a per-`deliver`
  epoch deadline (e.g. 250 ms wall-clock); guests that overshoot are
  trapped at the next safepoint, the trap surfaces in tens of ms, mark
  dead and continue. Rejected for v1 because the failover design that
  would tell the substrate what to do after the mark-dead step does
  not yet exist. Reasonable to layer on top of the abort policy later
  as defense-in-depth, but doesn't obviate the need for an explicit
  failure policy.
- **Drain timeout that just gives up without aborting.** When `drain_all`
  exceeds its budget, return early; the component eventually finishes
  trapping and decrements pending on its own. Rejected because the
  trapping component is still wedging its mailbox and (depending on
  what wedged it) may continue to wedge. Without abort the substrate
  silently degrades; with abort the operator sees the failure
  immediately.
- **Per-component abort policy (load-bearing vs. incidental).** Tag
  components at load with whether their death should abort the
  substrate; let "incidental" components die without taking down the
  whole process. Rejected as premature — there is no current notion of
  load-bearing vs. incidental, and adding one before we have a
  multi-component workflow would be designing for a use case we
  haven't validated.
- **Better progress logging during the freeze, no abort.** Have
  `drain_all` periodically log "still waiting on mailbox X, Y ms
  elapsed" while it spins. Rejected because it improves the diagnostic
  but doesn't fix the experience: the application is still frozen, the
  operator still has to wait for the trap to surface, and the eventual
  recovery is the same silent flip-and-continue.
- **Keep current behavior, document the freeze.** Rejected for the
  obvious reason: documentation does not change the dev-loop cost.
