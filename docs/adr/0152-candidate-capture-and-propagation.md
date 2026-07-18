# ADR-0152: Candidate capture and propagation

- **Status:** Proposed
- **Date:** 2026-07-18

## Context

ADR-0149 defines the member line — Construct → Verify → Refine → Review — over a *candidate*: the source tree a model-lane attempt produces against the sealed base. ADR-0150 defines how bloom digests map to real git objects (persisted, format-tagged correspondence) and pins the trust boundary: workers never hold credentials, and a worker cannot verify its own candidate. ADR-0151 defines evidence admission — a verdict binds to an exact subject digest via `Evidence.subject`, and `ResolutionClaim` carries the candidate digest its evidence must validate.

The vocabulary is in place; the candidate itself has no home. Concretely, in the shipped system:

- A Construct/Refine run's working-tree changes are read as a boolean (`produced_candidate`) for the substantive-conclusion gate, then discarded when the run worktree is removed. No tree or commit object is ever created from the work.
- `StageProgress` (the per-member cursor) stores `{stage, attempts}` only. The reducer has no state slot for "the candidate this member is currently at".
- `Fact::AttemptCompleted` carries `{passed, evidence}` only, so the reducer cannot learn a produced candidate's digest from a completed attempt.
- Both dispatch builders pin `Transformation.checkout` to the bloom's sealed base for every stage — `Transformation::checkout`'s own rustdoc concedes the per-member checkpoint as future work. Verify therefore verifies the *base*, and Review diffs the base against itself and sees no candidate at all.
- The executor driver stamps `DispatchRecord.{scope_revision, candidate, displayed_digest}` all from the same subject input, so the `ResolutionClaim` minted on a Review pass names the scope revision as its "candidate".
- `SourceBackend::integrate` — which turns a candidate tree into a commit on the bloom's integration branch under a checkpoint guard — has no caller in the resolution flow, and nothing produces the `Fact::Resolve { tree, head, .. }` that `Decision::DispatchLand` consumes. A bloom can currently record resolutions and "land" a head identical to its base.

Every downstream property of the bloomery — meaningful verification, meaningful review, evidence that actually vouches for the work, and a landing head that contains the work — depends on closing this gap. It is the prerequisite for the agreed review restructuring (whole-bloom aggregate review) and for retiring the GitHub pipeline (#3564 chain).

Constraints carried from prior ADRs:

- **Zero-secret workers (ADR-0150).** The Actions lane runs with `contents: read`; a runner cannot push a candidate anywhere. Model lanes (Construct/Refine) run on the local executor, where the *host* — not the child — holds credentials.
- **Exact-digest evidence (ADR-0149 §supersession, ADR-0151).** Refinement produces a new candidate and old evidence never validates it. Whatever identifies a candidate must change whenever its content changes.
- **Correspondence, not embedding (ADR-0150).** Bloom digests resolve to git objects through the persisted `git_correspondence` map; git shas never enter the content-addressed vocabulary.
- **Appended wire evolution.** Journaled facts replay; enum variants are appended to keep discriminants stable. Changing a fact's field set is a journal-format break.

## Decision

### A candidate is two correspondence-mapped digests

A captured candidate is identified by a `CandidateRef { tree: Digest, checkout: Digest }`:

- **`tree`** — the identity of the work: a digest whose correspondence row resolves to the produced git *tree* object. This is the digest evidence binds to, the digest `ResolutionClaim.candidate` names, and the digest `SourceBackend::integrate` consumes. Content-derived, so a Refine that changes anything yields a new `tree` and every prior verdict stops validating — the ADR-0149 supersession property falls out of the identity choice.
- **`checkout`** — the vehicle: a digest whose correspondence row resolves to the *capture commit* wrapping that tree (parent = the attempt's own checkout commit). This is what `Transformation.checkout` carries for downstream stages; executors resolve it exactly as they resolve the sealed base today.

Both digests are derived from the captured git object ids and persisted as format-tagged `git_correspondence` rows, per ADR-0150. The split mirrors the two axes `Transformation` already has (evidence-binding input vs. checkoutable commit) and the two axes `IntegrateOutcome` already returns (`tree` vs. `head`).

### The host captures; the worker never does

Capture is a host-side step in the local executor, after a model-lane child exits and before its worktree is released:

1. Stage and commit the run worktree's changes as the **capture commit** — authored under the bloomery's own fixed identity, parent = the commit the run checked out. A run with no changes captures nothing (`CandidateRef` absent), which the existing substantive-conclusion gate already fails.
2. Record the tree and commit correspondence rows in the store.
3. Push the capture commit to the bloom's candidate ref — `refs/heads/bloom/<hex>/candidate/<workpiece>` alongside ADR-0150's integration ref, force-updated because refinement supersedes — using the host's credentials.

The child process (the model lane) never stages, commits, or pushes: candidate capture happens above it, in the trust domain that already holds the token, keeping ADR-0150's boundary intact. The push is what makes the candidate reachable by zero-secret Actions runners (their checkout fetches the sha from the hosted repo) and by the API-side `integrate` (which resolves the tree server-side).

### The reducer learns, stores, and re-targets

- `Fact::AttemptCompleted` gains `candidate: Option<CandidateRef>` — populated by the driver from the capture step on model-lane completions, absent on mechanical lanes and failed runs.
- `StageProgress` gains `candidate: Option<CandidateRef>` — the member's current candidate, written when a passing Construct/Refine completion carries one, carried forward otherwise.
- The dispatch builders resolve per stage from the cursor: a member with a candidate dispatches Verify/Review with `inputs[0] = candidate.tree` and `checkout = candidate.checkout`; Refine likewise starts from the candidate it is refining. Construct (no candidate yet) keeps today's `inputs[0] = scope_revision`, `checkout = sealed base`. A stage-retry re-dispatches against the member's current candidate, not the bare base.
- `DispatchPayload` gains `scope_revision: Digest` and `candidate: Option<Digest>` (the tree), so the driver stops inferring record fields from `inputs[0]`: `DispatchRecord.scope_revision` is always the true scope revision, and `displayed_digest = candidate` when present, else `scope_revision`. The intake's existing name-claim check (`claimed subject == displayed`) then binds post-Construct evidence to the candidate with no contract change, and the `ResolutionClaim` minted on Review pass names the real candidate — `reduce_integrate`'s `evidence.validates(&claim.candidate)` becomes a meaningful check.

These are journal-format changes to appended-evolution types. The position: break them now, once, together. Bloomery journals are per-developer instance state (ADR-0150) and every existing journal is migration-trial throwaway; there is no compatibility surface to preserve pre-1.0, and holding the fact schema hostage to trial databases would buy nothing.

### Resolution drives integration

A new host driver closes the `Fact::Integrate` → `SourceBackend::integrate` gap: when a bloom's members have all recorded resolutions, the driver folds each claim's candidate tree onto the bloom's integration branch in member order (each `integrate` call CAS-guarded on the prior checkpoint, per the port contract), then admits `Fact::Resolve` with the final `IntegrateOutcome`'s `{tree, head}`. `Decision::DispatchLand` and the existing land driver consume it unchanged. Integration happens once per bloom, at resolution — member-line stages read candidates from candidate refs and never touch the integration branch.

This ordering also positions the agreed whole-bloom aggregate review: the integrated head is the tree an `AggregateReview` stage checks out and judges. This ADR builds the substrate for that stage; the stage itself and the findings→Refine re-entry loop are their own change.

## Consequences

- Verify, Review, and Refine operate on the actual work. The review lane's "empty diff = no candidate = finding" rule becomes a live tripwire instead of the guaranteed outcome.
- Evidence and claims become honest: a green Verify vouches for the candidate tree it names, and superseding a candidate invalidates exactly the evidence bound to the superseded tree.
- A landed bloom's head contains the members' work — landing stops being a fast-forward of the base to itself.
- The hosted repo gains per-member candidate refs and per-bloom integrate commits under the existing `bloom/<hex>/` namespace; they are working state, deleted with the bloom's other refs at cleanup.
- One coordinated journal-format break (`AttemptCompleted`, `StageProgress`, `DispatchPayload`); existing trial journals are discarded, not migrated.
- Capture adds a host-side git commit + push to every successful model-lane run — one network round-trip, on the host's credentials, bounded by the run's own diff size.
- Follow-on work enabled: the aggregate-review stage over the integrated head; the reducer stage-loop change feeding review findings into a single Refine re-entry; retiring the per-member blind Review retry.

## Alternatives considered

- **Candidate = one digest (the commit only).** Rejected: `integrate` consumes a tree, evidence should bind to content identity rather than a commit wrapper (two capture commits of the same tree against different parents would otherwise be different "candidates"), and the one-digest form would re-collapse the two axes `Transformation` deliberately separates.
- **The child commits/pushes its own candidate.** Rejected: hands write credentials to the model lane, violating ADR-0150's boundary, and makes the capture trustworthiness depend on the worker rather than the host.
- **Ship the candidate through the evidence artifact (bytes or name segment) instead of git refs.** Rejected: the intake's name-claim contract is deliberately fetch-free and verdict-shaped; a candidate is a source tree, which git already stores, dedups, and transports — and Actions-lane downstream stages need a fetchable commit regardless.
- **Integrate each member's candidate as it passes Review (rolling integration branch), dispatching later stages from the integration head.** Rejected for now: it serializes the member line behind integration order and makes every member's Verify depend on unrelated members' work; per-member candidate refs keep members independent until resolution, matching ADR-0149's per-member line. Revisit if cross-member conflicts at resolution prove common.
- **Migrate existing journals across the fact-schema change.** Rejected: pre-1.0, per-developer throwaway state; a migration would be dead code the day it lands.
