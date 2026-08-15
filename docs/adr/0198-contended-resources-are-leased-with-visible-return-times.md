# ADR-0198: Contended Resources Are Leased with Visible Return Times

- **Status:** Proposed
- **Date:** 2026-08-15

## Context

The machine that runs the fleet has a handful of genuinely scarce resources:
build capacity (the measured binder — cold target directories near 36 GB and
build-directory locks that serialize unmanaged concurrent builds), the
per-slot target directories that solved that serialization, per-harness API
concurrency, memory, and eventually the GPU. Today one number stands in for
all of them: a global concurrent-lane ceiling. It is blind in three ways. It
conflates unlike resources, so a lane holding only a cheap review seat counts
the same as one mid-`cargo build`. It admits or refuses and nothing else —
a refused agent learns nothing about *when* to come back. And it governs only
bloom lanes, while direct agents, interactive sessions, and future engine
actors contend for the same machine unmanaged.

ADR-0196 made dispatch dependency-aware: a member runs when the work it
builds on exists. This record makes dispatch resource-aware: a member runs
when the capacity it needs exists — and when it cannot run yet, it knows
when its turn begins.

The primitives are already in the system's vocabulary. A checkout with a
deadline and reclamation is the claim shape the journal uses for refs
(ADR-0179 releases; heartbeat eviction; the overdue reaping the operator
surface reads). Predicting a holder's release from observed history is the
per-handler EWMA the engine's cost tables already keep. What is missing is
only the librarian: one place that owns the copies, grants the checkouts,
runs the clock, and answers the queue.

## Decision

Contended resources are leased from a per-machine librarian, over the same
MCP/REST surface every agent already speaks.

### The registry

Each machine declares its resources in configuration: a name, a capacity
(how many copies of the book exist), and a mode. `cargo-build: 6` is six
shared checkouts; `slot-3-target: 1` is exclusive; `harness-grok: 4` caps
concurrent grok children; `gpu: 1` reserves the render device. Capacities
are host facts, so they live in host configuration — argv/env over defaults,
like every chassis knob — not in sealed bloom state.

### The lease

A checkout grants `{resource, holder, granted_at, expires_at}`. Holders
renew by heartbeat while legitimately working; a lease that expires
unreturned is reclaimed by the reaper, and return-after-expiry is an
idempotent no-op. Reclamation is cooperative-plus-reaping, the fleet's
existing posture: the enforcement point is the tool wrapper (the lane
environment carries the token; the build and harness arms check validity
before the expensive step), and a holder that cannot be reclaimed
quarantines its resource visibly rather than silently shrinking capacity —
the slot-quarantine shape, generalized. This is a scheduling contract among
our own agents, not a security boundary, and the record says so plainly.

### The queue and its forecast

This is the load-bearing clause. A denied checkout returns a queue position
and a return-time forecast, computed from the earlier of each holder's
`expires_at` and its predicted release — an EWMA of observed hold times per
resource per stage, the `actor_cost` pattern applied to leases. Waiters
subscribe rather than poll. The consequence is that waiting becomes
schedulable, at both levels that plan. The ADR-0196 readiness fold consumes
the forecast and prefers dispatching members whose leases are free now,
interleaving cheap stages into the gaps under expensive ones, instead of
blocking wide work behind a blind semaphore. And the forecast is equally
addressed to the waiting agent itself: a denial with a return time is a
planning input, not a rejection. An agent told its build slot frees in four
minutes sizes work to that window — reads the next work order, runs the
cheap half of its verification, drafts the report it will need anyway — and
arrives at its turn already prepared. The difference between a semaphore
and a library is that the library tells you when your turn begins, and an
agent that knows its wait can spend it.

### One librarian, every agent

The librarian is a capability on the machine's hub, exposed as MCP tools
(acquire, renew, release, status) and REST for the coordinator. Bloom lanes
receive their tokens through the lane environment; direct agents and
interactive sessions call the same tools; an engine actor can hold a GPU
lease the same way a verify lane holds a build lease. The bloomery
coordinator is the first consumer: a lane's admission becomes the set of
leases its next stage needs — slot, build, harness seat — and the global
lane ceiling becomes a derived consequence of registry capacities rather
than a configuration constant.

### The evidence

Grants, releases, expiries, and per-checkout wait times journal as evidence
where the holder already journals (lane leases into the bloomery journal;
other holders into the librarian's own log). Contention becomes a measured,
priced quantity in the calibration ledger — "how long did work wait, on
what, behind whom" — so capacity changes are made on data, the same way
seat changes are.

## Consequences

- Many light agents and few heavy ones coexist on one machine, because
  admission counts what each actually holds rather than that it exists.
- Waiting stops being wasted: agents with a forecast reorder their own work,
  and the DAG scheduler packs the machine instead of convoying behind the
  widest resource.
- Expensive provisions — the GPU, an end-to-end substrate test rig — become
  shareable fleet resources instead of things only one hard-coded consumer
  may touch.
- The lane ceiling, slot assignment, and per-harness caps unify into one
  mechanism with one operator surface; `MAX_CONCURRENT_LANES` retires into
  a derived number.
- Cost: a new capability with liveness obligations (heartbeats, reaping,
  quarantine) and one more thing the boundary tooling must keep honest;
  forecast quality starts cold and earns trust as EWMAs fill.
- Follow-on work: the librarian capability and its MCP tools; lane-side
  token threading and wrapper checks; the coordinator's admission rewrite;
  scheduler consumption of forecasts; calibration columns for wait time;
  registry configuration for eve.

## Alternatives considered

- **Keep global semaphores** — one number per concern. Rejected: blind to
  which resource is scarce, silent to waiters, and scoped to bloom lanes
  only; this record exists because that ceiling is already the bottleneck.
- **OS-level enforcement (cgroups, device permissions)** — real containment
  but no queue, no forecast, no scheduling signal, and invisible to agents;
  useful someday *under* the librarian, useless as the librarian.
- **Token-bucket rate limiting** — the right shape for per-minute API
  budgets, the wrong shape for unit resources; a harness seat is a copy of
  a book, not a refill rate. A bucket can later back a lease's capacity
  where a provider's limit is genuinely rate-shaped.
- **Scheduler-private accounting (no MCP surface)** — the coordinator could
  lease internally without a librarian anyone else can call. Rejected: the
  machine's contention is fleet-wide, and a resource only the scheduler can
  see is a resource every other agent still tramples.
