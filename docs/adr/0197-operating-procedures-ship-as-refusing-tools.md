# ADR-0197: Operating Procedures Ship as Refusing Tools

- **Status:** Proposed
- **Date:** 2026-08-15

## Context

The bloomery's correctness machinery — sealed configs, journaled decisions,
gated landing — is code, but much of its *operation* has lived as prose:
runbook knowledge carried in session notes and agent memory, re-executed by
hand at each roll, upgrade, seal, and recovery. The failure record of one
week shows what that costs. The day roll failed four days running, each time
on a different improvised step: a repository merge setting nobody had
flipped, a wire reshape that bricked the coordinator at boot, a store copy
taken without its WAL sidecars that silently replayed a stale prefix, and a
day branch whose forward-sync merge commits made the sync-back structurally
unmergeable while the check-wait misread GitHub twice. Recovering one
stalled bloom cost an hour, most of it re-deriving facts the repository
already knew (#4997's gap statement). Each incident ended the same way: an
agent diagnosed correctly under pressure and hand-built the recovery — and
the next agent, or the same one a day later, faced the same cliff again.

The pattern is not agent failure; the pattern is that the procedure had no
owner in code. Where a procedure did become a tool, the failure class
closed: the roll's preconditions screen names every violated condition at
once before anything moves; the operator command surface reads wire-encoded
state and prints signed overdue times instead of leaving operators to unpack
integers by hand; the seal command authors typed bodies instead of
hand-rolled JSON. Where a tool lived *outside* the repository — host-side
scripts on the coordinator machine, hand-copied helper binaries — it
drifted from the code it operated and produced its own incident class.

## Decision

Every recurring operating procedure ships as a tool in this repository, and
the tool refuses rather than trusting.

- **The tool is the runbook.** Roll, seal, supersede, upgrade, recovery,
  and any procedure an operator performs more than once exist as an `xtask`
  arm or an operator-CLI verb. Prose describes intent; the tool encodes the
  steps, in the repository, versioned with the code it operates. Host-side
  unversioned tooling is the anti-pattern this retires.
- **Refusal before motion.** A tool checks every precondition before its
  first side effect and reports all failing preconditions at once, each
  named with the state that violates it — the roll screen's shape is
  normative. A refused invocation is a no-op; a procedure that cannot
  finish has not started.
- **Absence is "not yet", never a verdict.** A tool that waits on external
  state (a check run, a child process, a replayed journal) distinguishes
  "not reported yet" from "reported and failed" and from "reported and
  passed", and times out with the distinction named. Reading an absent
  signal as either verdict is the class behind the roll's two check-wait
  misreads.
- **Prove state transitions on a copy first.** A procedure that replaces a
  running binary over a persistent store first replays a copy of that store
  (WAL sidecars included, row counts compared) under the candidate binary
  and compares the folded state to the live one. This — today a memorized
  script — becomes `cargo xtask bloom upgrade`: fold-test, deploy, restart
  through the supervisor, verify the observation advanced.
- **Tools state what they checked.** Output names each precondition
  verified and each fact observed, so a transcript of the invocation is
  itself the evidence an operator or a reviewing agent needs — no
  re-derivation.

Agent judgment remains for what tools cannot own: diagnosis of novel
failures, and the decision to run a procedure at all. The first time a
recovery is improvised is unavoidable; the second time is a missing tool,
and filing that tool is part of closing the incident.

## Consequences

- Operating knowledge stops living in per-agent memory and starts versioning
  with the system, so a new agent's floor is the tooling's ceiling — the
  mechanism by which one agent's burned lesson protects every later one.
- Procedures gain the same review, test, and CI surface as the rest of the
  code; a broken runbook becomes a red check instead of a 2 a.m. discovery.
- Follow-on work: `cargo xtask bloom upgrade` (the fold-test-and-restart
  arm); auditing the remaining host-side scripts on the coordinator machine
  into repository tools or deleting them; the roll hardening already in
  flight (#5003) completes the roll's conversion.
- Cost: tool surface grows and each procedure change becomes a reviewed
  pull request rather than an edited note. That friction is accepted — it
  is the point.

## Alternatives considered

- **Better runbooks** — richer prose, checklists, memory records. Rejected:
  the week's failures all had accurate prose available; prose cannot refuse,
  and every reader re-implements it under pressure.
- **A supervising meta-agent that executes runbooks** — rejected: it moves
  the improvisation up a level instead of removing it, and its knowledge
  drifts exactly as prose does.
- **Tools outside the repository** (host scripts, operator dotfiles) —
  rejected by direct experience: they drift from the code they operate and
  are invisible to review and CI.
