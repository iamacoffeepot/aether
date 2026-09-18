# Reactor-set work orders

- **Status:** Proposed; split and approve each issue before implementation
- **Date:** 2026-09-17
- **Design:** [Journal-selected reactor set](bloomery-reactor-set.md)
- **Decision:** [ADR-0223](../adr/0223-journal-selected-reactor-set.md)

These are bounded planning slices, not dispatched work. Re-ground each
against current main and freeze its declared surface when scoped. Keep the
reactor-set arc within Bloomery application crates. None authorizes engine
or chassis changes.

## Sequence

| Order | Deliverable | Proposed surface |
| --- | --- | --- |
| W1 | Atomic move-head command on journal owner | `aether-bloomery-journal-actor`, `aether-bloomery-kinds` |
| W2 | Requests, receipts, attribution, and native driver | `aether-bloomery-kinds`, `aether-bloomery-program`, `aether-bloomery-reactor` |
| W3 | Executor selection without a required member | `aether-bloomery-kinds`, `aether-bloomery-view` |
| W4 | Native feeder and side-by-side routes | new `aether-bloomery-feeder` |
| W5 | Activate and retire program executors | feeder and kinds crates |
| W6 | Genesis through the application layer | feeder and journal-owner crates; chassis integration deferred |
| W7 | Restart and boundary trace tests | feeder and driver tests |

Each order depends on the preceding contract unless a scoped issue proves
it can proceed independently. ADR-0223 and this packet are the docs revision
of the earlier W8; they do not dispatch W1–W7.

## Acceptance per slice

**W1 — Move-head command.** One typed, fenced request stages the immutable
artifact and appends its head move in one journal batch. Reply with the
committed sequence or the actual head when the fence loses. This is the
author-facing update path, not raw batch mail. Prove failure leaves neither
a partial artifact binding nor an apparent move.

**W2 — Durable request and receipt path.** Define `Requested`, `Transition`,
and `Fault` kinds and a native driver that resolves an input closure and
sends tracked execution to a selected executor. Reactor output must carry
trigger cause and rule identity to the writer. Native feeder requests carry
a stable native origin and journal cause instead of a WASM reactor identity.
Prove the writer deduplicates each source identity and cause from the journal
fold, and every receipt names one request. Timeouts and crashes leave a
recoverable open or faulted attempt. Require at-least-once execution and
durable-record deduplication; tracked mail alone proves neither.

**W3 — Executor selection.** Define a set and prefix selector parallel to
the reactor selector, with an empty set valid. Bind a declaration separately
from its executor artifact. Native executors may register by declaration at
boot until a later guest trampoline is designed. Preserve historical
selection and test membership and head moves in one batch.

**W4 — Feeder.** Page journal entries contiguously; fail visibly on a gap.
Rebuild heads, receipts, and routes on restart. Cache independent instances
by `(cluster head, artifact digest)`; use the existing load and drop mails
and cluster export. Fold-only warmup must not emit reactions. Route live
entries with a token that distinguishes move sequences, await
`EvaluatedResult` in order, and never evict a routed instance. Determine
collision-safe runtime names and the safe reuse rule for `A → B → A`
before claiming this slice complete.

**W5 — Lifecycle programs.** The native feeder requests and executes
`core.cluster.activate { cluster, artifact }` and
`core.cluster.retire { cluster }` through W2's durable path. Activation
returns `Activated | Rejected { reason }`. An already proven active target
may answer idempotently after restart. Rejection does not undo the recorded
head move; it preserves the predecessor instance and carries the failed
interval forward for later live delivery. Process moves in order; a
correction is another head move, never an unrecorded route rewrite.

**W6 — Genesis.** Establish an empty `ReactorSet` or stage a first bundle
and its head atomically, then let the feeder converge. No required member
and no special executable are allowed. The application composition that
loads the journal owner and feeder, especially any chassis boot fragment,
is a separate future decision with its own surface and authorization.
Completion of W6 inside the current arc must stop at the application-crate
boundary and cannot claim an end-to-end chassis boot.

**W7 — Trace proof.** With an in-memory journal and scripted executors,
inject crashes around request, execution, receipt, load, warmup, and route
flip. Assert the exact final log and live delivery sequence. Cover one
receipt per request, derivation of open work, predecessor evaluation of
move `N`, successor live evaluation of `N+1..`, rejection and later-owner
delivery, stale replies in `A → B → A`, two moves in sequence, and identical
bytes under distinct cluster heads. Treat these as tests to build, not
properties already established by current handlers.

## Disposition and exclusions

PR #6141 landed the prefix selector. PRs #6140, #6142, #6143, and #6147
were reverted by #6172 on main `b4b7e41`. PRs #6170, #6148, #6165,
#6163, and #6146 are closed. Do not resurrect the kernel,
required-member check, in-place replacement transaction, protected slot,
held preparation, or pending-ring queue as prerequisites of these orders.

Ordinary component replacement and private inline-child reconstruction
remain separate engine hygiene. Guest program trampolines, a `Pure` linker,
`Sampled` WASM executors, and large-closure transport are later decisions.
No order here authorizes edits under `crates/aether-substrate`,
`crates/aether-actor`, `crates/aether-actor-derive`,
`crates/aether-component`, `crates/aether-behavior`, or
`crates/aether-chassis-*`.
