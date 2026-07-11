# ADR-0144: Tick-native sim intent and fact vocabulary

- **Status:** Proposed
- **Date:** 2026-07-10

## Context

The authoritative game simulation is a fixed-tick turn game: state N+1 is a pure
function of state N and the intents binned into tick N — `state(N+1) = f(state(N),
intents(N))` — applied atomically, deterministically, with all game time denominated
in ticks. Two independent consumers need to speak to that simulation, and they need
to agree on the mail vocabulary before either is built:

- A **reference turn-sim component** (this ADR's motivating build, in `aether-kit`)
  that runs the loop over a toy tile world and is drivable in TestBench with
  `BenchOp::advance` as the turn driver.
- A future **player session tier** over tcp that decodes untrusted inbound frames
  into intent kinds — where the session actor's handler table is the allowlist and a
  frame names a *kind*, never a recipient — and assembles a per-tick outbound fact
  bundle per connection.

Nothing named `aether.sim` exists in the tree today (`git grep 'aether\.sim'
origin/main` is empty), so this is a clean vocabulary decision rather than a
migration. The forces:

- **Two directions, asymmetric.** Intents flow *up* (client → sim), one small kind
  per action, binned into the current tick. Facts flow *down* (sim → consumers), and
  a consumer that is live, lagging, or freshly joined must all reconcile to the same
  authoritative state — so the down direction needs more than a raw event stream.
- **Determinism is already load-bearing.** `aether-kit` positions are fixed-point
  octimeters precisely so the simulation is bit-exact across machines (the crate doc
  states this as the precondition for server authority and deterministic replay). The
  vocabulary must not reintroduce float or wall-clock time into game state.
- **Decoupling.** `aether-kit` depends on `aether-capabilities`, not the reverse, so
  the reference component sits *above* the session tier in the crate DAG. The
  vocabulary must not force the lower crate to import the higher one.

## Decision

Pin an `aether.sim.*` mail vocabulary as the tick-native contract between an
authoritative turn simulation and its consumers. The vocabulary is the contract; the
`aether-kit` reference component (actor namespace `aether.kit.sim`) is one
implementation of it.

**The vocabulary is a wire contract, not a Rust dependency.** The intent and fact
kinds are named (`#[kind(name = "aether.sim.…")]`) and schema-described; a consumer
speaks them by kind-id over the wire — the reference component carries them in its
`aether.kinds` custom section (ADR-0028/0032), and the session tier decodes inbound
frames against its handler table by kind-id (ADR-0033). Neither consumer needs to
`use` the other's Rust types. The kind *definitions* therefore live in the reference
component's module (`aether-kit`) without creating a dependency cycle: the session
tier in `aether-capabilities` (the lower crate) references the vocabulary as wire
names, never as imported types. If a future consumer ever needs the Rust types
in-process, hoisting the kind definitions to a shared lower crate is a separate,
additive move — this ADR does not preclude it, and records the seam.

**Intents flow up, binned per tick.** A consumer issues intent kinds to the sim's
mailbox; the sim collects them into the current tick's bin and applies them at the
next turn step. Within one tick's bin, a later intent for the same entity supersedes
the earlier (last-writer-wins) — the sim never applies two conflicting intents from
one entity in one turn. The turn step is driven by the substrate `Tick` stream the
sim subscribes in its `wire` hook, so one `advance(1)` steps exactly one turn.

**Facts flow down as a per-tick bundle with three forms.** Each turn produces one
`aether.sim.tick_bundle` keyed by its tick number, carrying:

1. **Trajectory events** — the granular per-entity deltas of that turn (spawned,
   moved from cell A to cell B, removed). The fine-grained "what happened" stream a
   live consumer applies incrementally.
2. **A state summary** — the authoritative post-turn snapshot of the entities in the
   consumer's interest set. A lagging or freshly joined consumer applies the summary
   and discards buffered trajectory it never saw. This is the superseding form: a
   summary at tick N is the ground truth for tick N regardless of which trajectory
   events reached the consumer.
3. **A supersession watermark** — `superseded_through`, the tick through which the
   summary makes prior trajectory redundant. A consumer drops buffered events at or
   below the watermark. This is what bounds consumer memory and makes reconnection a
   summary-application rather than a full replay.

The bundle is **atomic** (a turn emits exactly one, fully applied), **tick-ordered**
(bundles arrive in tick order and carry their tick), and **bounded** (the sim retains
a fixed-depth ring of recent bundles; a consumer asking for a tick older than the ring
gets a summary at the ring's oldest retained tick instead — the "you fell behind,
here is ground truth, resume here" path).

**Interest projection is a named seam, trivial in v1.** The bundle's entity set is
the consumer's interest projection; in v1 the projection is the identity (every
consumer sees every entity — "trivially large K"). The session tier owns per-connection
projection; the sim exposes the full set and the seam where projection attaches.

**Two read surfaces, one vocabulary.** The sim both *pushes* each `tick_bundle` to an
optional fact-sink mailbox named in its config (the live path a session tier binds
to) and answers a *pull* — `aether.sim.poll { since_tick }` replies the retained
bundles since that tick plus the current tick number. The pull path is the standalone
TestBench-drivable surface (drive intents, `advance`, poll, assert on the reply) and
the catch-up path for a reconnecting consumer; the push path is the live per-tick
emission. Both carry the identical bundle shape.

## Consequences

- The reference turn-sim component and the future player session tier share one
  agreed vocabulary, unblocking both to be built against a fixed contract.
- The session tier stays decoupled at the Rust level: it speaks the vocabulary as
  wire names against its handler-table allowlist, taking no dependency on `aether-kit`
  and — per this issue's scope — no dependency on the tcp capability. This issue is
  TestBench-drivable standalone.
- The three fact forms give a consumer a single reconciliation rule: apply the
  summary as ground truth, apply trajectory above the watermark incrementally, drop
  everything at or below it. This is the bundle contract (atomic apply, tick order,
  supersession, bounded size) the session tier assembles its outbound frames from.
- Determinism is preserved: game state stays fixed-point and tick-denominated; no
  form in the vocabulary carries float positions or wall-clock time.
- The vocabulary is deliberately minimal (spawn / move intents; move / spawn / remove
  trajectory; a flat entity-position summary). It is a reference, not the game — the
  toy world exercises the *shape* of the contract, and richer intent and fact payloads
  extend the families without changing the bundle contract.
- Open follow-on: if an in-process consumer ever needs the Rust kind types (not just
  the wire names), the definitions hoist to a shared lower crate. Recorded, not done.

## Alternatives considered

- **Raw event stream, no summary/supersession** — a consumer would replay every
  trajectory event from tick 0 to reconstruct state, with unbounded buffering and no
  clean reconnection. Rejected: the summary + watermark is what bounds memory and
  makes a lagging or freshly joined consumer a cheap catch-up.
- **Hoist the kind types into a shared lower crate now** — would let both consumers
  import the same Rust types. Rejected as premature: the session tier decodes by
  kind-id against its handler table and needs the wire vocabulary, not the types; the
  higher-crate home keeps the reference component self-contained, and hoisting stays
  available as a later additive move.
- **Per-intent reply instead of a per-tick bundle** — replying facts to each intent
  would break the atomic-turn model (facts are a property of the turn, not of one
  intent) and give no home to the summary/supersession forms. Rejected.
- **Drive turns by an explicit `aether.sim.step` mail rather than the `Tick` stream**
  — would decouple the sim from the substrate clock but duplicate a scheduler the
  substrate already owns and break `advance`-as-turn-driver. Rejected: subscribing the
  `Tick` stream makes `BenchOp::advance(n)` step exactly n turns for free.
