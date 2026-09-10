# ADR-0218: Contextual verification and eager integration

- **Status:** Proposed
- **Date:** 2026-09-09

## Context

Member verification and the final aggregate fold serialize repeated builds
and postpone interaction feedback. ADR-0217 added coalesced aggregate
pre-checks, while issue #5899 restored the author's session for Reconcile.
They preserve standalone member proofs and the deferred complete fold.

The observed two-member example took 52.963 seconds in one warm serial slot
and 32.310 seconds as a composition. Both members changed the same package.
This demonstrates one fewer changed-package/test-binary rebuild, not fleet
throughput or a universal waiting threshold. Existing slot source and target
paths are stable within each slot; they differ across slots. Cross-slot cache
effectiveness and production throughput remain separate measurements.

The next implementation must distinguish a logical verification obligation
from the physical execution that checks it. A group can fail for several
independent reasons, for an interaction, or because the host never reached
the tests. Its survivors must not retain an ejected contribution through a
dependent's ancestry. Eager integration also needs a durable partial head:
the final Resolve fact cannot represent an incomplete bloom.

The owner requested the complete implementation before rollout and
performance trials. This decision therefore includes contextual proofs,
survivor groups, and eager integration in the same coordinated change.

## Decision

### One optional sealed policy

`CoordinationPolicy` is absent for legacy blooms. It selects `Standalone`,
`WarmSerial`, or `Contextual` verification and independently enables eager
integration. It seals positive bounds for group size, serial lease service,
head movement, and reservation lifetime, and a finite attribution-probe
budget. These are resource and liveness limits, not learned timing estimates.
The policy also names the expected host class. The executor independently
supplies its configured class; shared work refuses a mismatch. There is no
implicit execution class for an enabled policy.

Ready requests are coalesced when a prover can serve them. There is no
deliberate wait for an arrival and no two-member ceiling. A running plan is
immutable; subsequent arrivals belong to the next plan. A warm serial lease
retains one existing private slot target while running each member's own
tree and gates. Admission is reassessed between requests, preserving service
for older work and each order's original deadline.

The deterministic policy sits behind the scheduling seam. A future oracle
may rank or size eligible work using measured costs. It may not weaken proof
identity, replace running inputs, reset deadlines, or invent attribution.

Contextual admission requires the aggregate contract to cover every declared
member gate under matching profiles, configuration, environment, and execution
limits. A request outside that coverage falls back to its standalone invocation.
Warm serial requests retain their own contracts and do not need aggregate
equivalence.

### Logical requests, physical runs, and proof classes

`MemberVerifyRequest` pins a `MemberPin`, attempt, original transformation,
profile, configuration, authoritative atomic `CompositionInput`, optional `ConstructContext`, and
`VerificationContract`. That per-member contract names the gate set, original
member obligations, diff base, executable invocation, sealed environment, and
host class. A `SharedRunPlan` retains its ordered requests and, in contextual
mode, the immutable `CompositionPlan`. Its separate `CompositionContract`
pins every original request and member contract in the plan's exact order,
the canonical aggregate gate identities, execution environment, and host
class. No member's delta contract stands in for the whole composition.

The composition contract also pins a `ContextualInvocationTemplate`: every
transformation field other than the subject tree and checkout, together with
the exact profile and configuration. Git preparation yields `SharedRunNode`,
which binds those two subject fields and complete member coverage. The
executor instantiates that template with this exact node and admits only a
matching dispatch. This binds the invocation before Git produces the tree
without hashing a placeholder subject that the executor later replaces.

```rust,ignore
enum ResolutionProof {
    Standalone(VerifyProof),
    InComposition {
        node: Digest,
        receipt: Evidence,
        plan: Digest,
        request: Digest,
        contract: Digest,
    },
}
```

A contextual pass proves the tested node under its retained composition
contract and the member's retained original request contract. It cannot
populate the legacy standalone member memo, impersonate a parent's evidence,
or make a different subset green. The member's own delta still faces its
original containment and authorization obligations. A union surface never
widens an ordinary member's approved scope.

Before preparing a composition, the source checks every member's exact pinned
delta against its original retained approved surface. Missing revisions,
mismatched pins, unreadable comparisons, and incomplete provider file lists
refuse preparation. An aggregate gate cannot substitute for this member-local
check, because it executes under the composition's identity.

The existing ADR-0200 ledger remains the only reusable fact store. Contextual
facts use an exact candidate (tree and checkout), ordered coverage, and complete
contract address, distinguished from parent closure keys. The physical plan's
retry ordinal is excluded from this tested-input address; otherwise distinct
executions could never supply independent observations for the same input.
Dispatch and contextual proof identities still bind the complete physical plan
and node. Changed checkouts remain cache misses even when their trees match.
Actual independent invocation reports pass through
`record_contextual_facts`; replaying one report twice is rejected. Missing,
unreached, disagreeing, and infrastructure observations supply no facts.
Independence is bound to the host's retained physical-step nonce. All internal
records in one artifact contribute at most one report per gate; changing an
artifact's ordinal or invocation id cannot create a second independent run.
The host compares separate retained full executions only when their exact
candidate, ordered coverage, and complete contract match. Subset and baseline probes cannot pose as
full-node reports.
The verifier's additive observation artifact records actual spawns, so an
older candidate executable can omit it without requiring new CLI flags.
Such a run can still produce its ordinary gate receipt, but cannot supply
facts that it did not report. Umbrella runs collect bounded per-gate documents
from the order's own output directory and keep their identities separate.
Malformed, mismatched, oversized, or symlinked files refuse collection.
Ledger admission accepts only exact gate
obligations declared by the sealed contract. Individual test observations
remain diagnostic evidence until a trusted test inventory can authorize
their fact identities; arbitrary reported names cannot mint facts.

A normal single-run receipt retains its ordinary completion meaning. Reusable
ledger facts additionally need independent observations; the host does not
repeat every successful suite merely to populate that ledger.

A fully settled contextual run may also answer the aggregate position for
the same selected head, coverage, and complete sealed contract. The reducer
derives this authority from its retained ordered requests and matching
node-bound receipts. Pre-check and final resolution can consume that authority
without another suite or a legacy member proof. A known-red head cannot use
an earlier green receipt to clear its repair obligation.

Before issuing a new full contextual invocation, the host may consume the
latest green fact for every declared gate under the exact node, complete
contract, and host class. A missing, red, or unknown gate makes this a cache
miss. Reuse retains the contributing ledger sequences in its own artifact;
it neither invents a physical invocation nor charges new execution cost.

### Durable execution and accounting

Journal values use the supported derived `Schema` vocabulary. Member maps use
string keys and retain typed workpiece identities in their records. Runtime
heap indirection has no custom `Box` schema implementation.

The journal, outbox, outstanding orders, and retained results own the mapping
from one physical run to its logical requests and subordinate probes. A
partial receipt is retained before the slot advances or a logical nonce is
consumed. Restart replays that state and resumes only unfinished work. One
shared nonce cannot be repeatedly fed through scalar evidence admission.

Failed verification diagnostics are retained under their exact evidence
digest before completion is consumed. Member and partial-head repairs read
that receipt's typed findings, including after restart; they do not depend on
a local evidence-file digest also naming an object in the artifact store.
Passing, empty, or executor-fault observations cannot seed code-repair findings.

Cancellation withdraws unstarted logical work while preserving completed
receipts and other still-needed work. An already-running composition retains
its immutable input until it finishes or is explicitly cancelled as a
physical operation. Joining a required final gate preserves the original
deadline and uses the existing outstanding-order lifecycle.

Before admission, the scheduler retains the exact proposal for every selected
logical request in one transaction. New arrivals and a changed head cannot
rewrite that proposal after a crash. Selection works over complete input
groups, including requests interleaved in the queue; exact versions already
covered by the base need no duplicate logical request. A retry gets a bounded
new execution attempt while keeping the original logical deadlines.

Physical cost is charged once to the bloom under its physical identity.
Member rows carry latency, outcome, and a physical-run reference. They do
not repeat or divide the charge. Probes are subordinate physical work, not
additional member attempts. Unknown observations and checkpoint movement
cannot spend a member's code-repair budget.

### Set-valued failure attribution and survivors

`next_batch_probe` is a deterministic policy over the immutable plan and
retained `BatchProbeReceipt`s. It requests bounded, dependency-closed
experiments from the same physical slot. A check is discriminated by two
different actual invocations. A red baseline is inherited. A red singleton
against a green baseline establishes a member cause. Both split halves are
examined; two green halves retain the failing interaction rather than
discarding it. An unsplittable dependency group also retains its interaction
scope. Missing observations or exhausted probe budget remain unknown.

A nominally green umbrella with a declared raw red gate enters diagnosis
before a contextual pass: legacy triage may have excused an inherited failure.
Baseline probes must actually execute the requested checks on their pinned
base or inherited head. An empty diff that skips a check supplies no verdict.
Narrowing precedes inherited-head diagnosis so a blocked baseline on one
member cannot hide an independent failure in another subset. Inherited and
unresolved scopes, and candidates carrying them, wait outside the survivor
group without being charged as established member defects.

```text
verify {A1, B1, C1, D1} -> confirmed failures {A1, C1}
repair A1 in A's author session; repair C1 in C's author session
prepare fresh node {B1, D1}; verify its outstanding obligations
offer A2 and C2 with any eligible compatible groups
```

The survivors keep their grouping, not a green inherited from the red
parent. A fresh node, namespace, and proof obligation follow ejection. Any
candidate whose inherited coverage contains an ejected version waits for
repair or rebase; it cannot carry removed code back through Git ancestry.
Ejecting a version does not withdraw the member's obligation from the bloom.

Interaction failures use the existing composition workpiece over the
identified parents, its derived union bound, and its own session. Its repair
candidate is an explicit contribution. Later merges must retain that
contribution rather than flattening it back to the unrepaired leaf refs.
The journal retains the survivors' exact ordered request identities until
their fresh plan settles or a member version changes. Queue interleaving
cannot split that group into independently verified leaf requests.

### Eager immutable heads

`IntegrationGeneration` binds the sealed base and active member revisions.
`IntegrationAppendPlan` pins an expected parent and a dependency-valid actual
append order of `CompositionInput`s. Each append promotes exactly one input;
that input may hold an entire contextual group. Its success is journaled
before the next input is selected, so a later collision cannot hide a
successful unrecorded prefix. Only one append may advance a bloom at a time.
Source results name the plan, generation, expected parent, and exact
versions; stale completions cannot advance a current head or revoke a newer
claim. Successful partial advances are journaled independently of Resolve.

The source merges exact candidate checkouts in private generation/plan
namespaces and checks tree correspondence. Git merging is not associative:
the recorded actual order is authoritative. Final resolution adopts the
selected root; it does not rebuild the leaves in sealed order and assume
the result is equivalent. Replacement, withdrawal, or ejection invalidates
inherited contexts and derives a fresh generation as required. A checked
generation epoch changes even when a replacement candidate keeps the same
scope revision; a polluted namespace cannot be reused by that replacement.

When the current head is an ancestor of the input, a compare-and-swap
fast-forward preserves the input's exact verified tree and checkout. The
integration node still records its own generation and append plan; aggregate
proof reuse matches the candidate, ordered coverage, and full contract rather
than equating the integration node's address with the shared-run node's address.

Contextual verification retains these actual input roots even when eager
integration is disabled. Before final readiness, it advances only a verified
input needed to release an unstarted dependent, then admits that dependent
against the resulting head. Speculative head pre-checks wait for final
readiness in this mode. Final resolution adopts the retained root and never
reconstructs an interaction repair from the original member leaves.

A late Construct can start on the current eligible head. Its order records
the sealed bloom base separately from the inherited head and exact member
versions. Dependencies become ready when their current revisions are covered
by that head. Known-red heads block new inheritance. A pending head is
explicitly unproven context; failures inherited from it require baseline
diagnosis, not automatic blame on the new member.

Construction admission has two durable steps. A queued request can be refreshed
while lanes are busy; when capacity is available, the host asks the reducer to
admit that exact request against the current eligible head. Only the journaled
admission may dispatch the author order. Replayed admissions retain their nonce
and original clocks, and a stale request cannot submit an order against an old
head. Running author orders retain their immutable inherited context.

If withdrawal or replacement invalidates a contribution already present in an
admitted Construct order's inherited head, the reducer holds the bloom and
retains that order's context, admission nonce, and captured work. It neither
relabels the authored checkout as based on the new head nor charges the author
with a reconciliation failure. The operator must rescope or supersede the
affected work before releasing the hold; release alone cannot prove that the
removed ancestry was stripped. Automatic recovery requires a source operation
that can replay only the exact old-head-to-candidate delta onto the new head.

An exact current red pre-check schedules a bounded repair of that partial head
under the composition workpiece. An interaction discovered during shared
verification uses the same explicit repair path for that tested node. Each
plan retains its exact parent inputs, including previous interaction repairs,
and the resulting candidate must be verified before promotion. A repair cannot
make progress by silently reconstructing unrepaired leaves.

### Verify the prepared repair and bound head movement

Reconcile resumes the member's own journaled author session. Before Verify,
`CandidatePreparationPlan` merges its returned authored candidate `R` onto
the recorded target head `H`, producing `PreparedCandidate P`. A residual
merge conflict returns to reconciliation without a false Verify pass.
`Standalone(P)` proves the prepared tree; `R` never receives `P`'s evidence.
A different final root `Q` still needs its aggregate proof.

Repair displacement is counted separately from code failure. After the
sealed movement bound, a durable `StableHeadReservation` pins one head
through preparation, verification, and promotion. New author work can
continue while append promotions wait. The reservation records its owner,
generation, head, original deadline, and named hold. A durable wakeup expires
it; one quiet poll or an in-memory lock is not a stability guarantee.

### Coalesced feedback, previews, and visibility

ADR-0217's newest-pending pre-check lifecycle consumes the selected eager
head. Superseded pending heads are skipped; running inputs remain immutable;
required work has priority. Exact final joins and matching aggregate proofs
reuse the existing lifecycle and ledger. A stale result can retain evidence
for the tree it tested, but cannot color the newest head green or red.

`PrecheckPlan` pins the selected checkout in its base field and retains the
head's actual member coverage order and sealed aggregate gate set. The
aggregate invocation independently keeps the sealed bloom base as its diff
base. Its `PrecheckNode` is bound directly to the already
materialized head's tree and checkout; eager mode does not queue another
leaf fold. A changed root produces a new plan and node even when member coverage is
unchanged. This preserves explicit interaction repairs and the source's
recorded merge order without changing the legacy pre-check wire types.
A passing pre-check cannot clear a separately established red verdict on
the same head; repair and promotion own that transition.

Construction checkpoints are immutable, bounded observations captured
without changing the author's checkout or index. Their merge compatibility
is provisional and version-bound. A scheduled group rechecks the real
evolving multi-way merge. Preview metadata never supplies verification
authority, and there is no target directory for every possible subset.
Only an exact current conflict hint may influence selection; stale generations,
checkpoint versions, admissions, and request pins are ignored. A clean preview
does not bypass source preparation.
Live plans, outstanding orders, and retained successors keep their pins until
cleanup can prove they are no longer needed.

The board and API expose the selected head and coverage, each member's
folded/collided/reconciling state and target head, reservation, pre-check
freshness, shared physical identity, and member latency.

**Membership remains atomic at Resolve.** The selected root must cover every
active member's current contribution. Eager folding does not land members
early. Independent landing changes membership and supersession policy and
is outside this decision.

### Compatibility and rollout

Legacy journal shapes retain their encodings. New decisions and facts append
to the vocabulary, with explicit upcasts from the preceding schema stamps.
New snapshot state derives only from journaled decisions. Positional view
rows retain explicit prior-shape decoders and frozen fixtures.

The feature remains disabled unless a future bloom seals its policy. This
implementation does not modify a running bloom, restart a service, or enable
production speculation. Correctness checks are part of implementation;
deployment, live acceptance, and throughput/cache experiments follow the
complete implementation and an operator decision.

## Consequences

Warm serial execution can save compilation without changing proof meaning.
Contextual groups can share a build and keep useful survivor groups, while
eager heads shorten collision feedback and the final integration tail.
The design makes those savings measurable without reporting moved work as
eliminated work or charging shared execution several times.

These gains require durable lifecycle state, explicit inherited provenance,
and conservative uncertainty handling. Failed groups may spend bounded
probe work and need fresh survivor verification. A running stale pre-check
still consumes real resources. Throughput benefit remains to be measured on
the application after the full path is implemented.

## Alternatives considered

- Distribute a composed green as standalone parent proofs: proves untested
  trees and makes later ejection unsound.
- Attribute by a diagnostic's path owner: identifies a suspect, not a cause,
  and loses independent or interaction failures.
- Reuse a mutable old group after ejection: can retain removed code through
  the branch or inherited ancestry.
- Restart young builds for arrivals: wastes completed work and changes the
  subject of an already-issued immutable plan.
- Wait 20.6 seconds: treats one experiment's saving as a universal deadline.
- Allocate targets per subset or preview: grows disk use with speculative
  combinations and discards existing slot warmth.
- Keep a second contextual receipt database: duplicates the journal,
  retained-result lifecycle, and ADR-0200 proof ledger.
