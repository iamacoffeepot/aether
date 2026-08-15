# ADR-0199: The Bloomery Owns Its Source and Its Work Orders

- **Status:** Proposed
- **Date:** 2026-08-15

## Context

Every authority the bloomery answers to sits on GitHub today. Work orders are
GitHub issues, read by intake. The git object store is the GitHub git-data
API — `GitSource` is generic over a `GitDataApi` client, and the one client in
production speaks GitHub REST. Approvals are plaintext markers in issue
bodies, so the trust behind a sealed approval reduces to "who edited the
body." And the outward mirror (ADR-0149) projects bloom state onto the same
host that owns all of those inputs.

ADR-0149 drew the boundary correctly: the core is digest-addressed, no core
module names a GitHub type, and the adapters live in their own crate. What
this record changes is which side of that boundary holds the authority.

Three forces push it inward.

**Trust.** The system is moving toward running autonomously, and an
autonomous machine's inputs must each be attributable to a signer or to the
machine itself. An issue body is neither: it is mutable text on a third-party
host, and the approval marker inherits whatever edit history GitHub shows.
The signing machinery that could carry this properly already exists
(ADR-0179), but it has nothing to sign so long as the work record lives in
issue bodies.

**Testability.** The executor's real-git path — checkout materialization,
diff-base resolution, worktree management — runs only against the remote
authority, so no hermetic end-to-end test of the coordinator has ever been
possible. The bare-tree defect class fixed in #5025 and #5027 reached
production for exactly that reason: the code that mishandled a tree-shaped
order digest was reachable only in a live fleet against live GitHub. A fold
replay validates the reducer; nothing validates the executor's git handling
under `cargo test`.

**Operations.** Every object read, every candidate fetch, and every landing
round-trips a third-party API, coupling pipeline latency and availability to
rate limits and outages the system does not control. Lanes fetching from a
repository on their own host are faster and never down.

## Decision

The bloomery's source of truth moves inside the fleet: a first-party git
repository and a first-party work-order store, both on the fleet host.
GitHub keeps a replica, and replication is strictly one-directional.

### The source authority

A bare repository on the fleet host becomes the authoritative object store
and ref namespace. `GitDataApi` gains a local implementation over it, and
everything above the trait — the ref discipline, compare-and-swap landing,
the correspondence handle — comes along unchanged, because the port contract
is already digest-addressed and already treats branch names as working
handles rather than identity. Lanes fetch from the local authority.
Landing CAS-updates local refs.

### The work-order store

Work orders move into a first-party store beside the coordinator's journal.
Intake reads it instead of GitHub issues. Scope revisions, approvals, and
operator statements become rows signed with the fleet's ADR-0179 keys, so an
approval is a signature the machine verifies rather than a marker anyone with
edit access can type. The unsigned body-marker convention retires with the
migration.

### One-way replication

After each land, the authority pushes a mirror of its refs to GitHub, and
the ADR-0149 projection continues to post the human-readable view — receipts,
status, the work record's public face. That is the entire relationship.
Anything on GitHub that did not arrive by the mirror push is not input to
the system: there is no admission path, no sync-back, and no state the
coordinator reads from the replica. The quarantine boundary this leaves is
enumerable — every input to the system is either signed by the operator or
generated inside the loop — which is the property an autonomous machine's
audit depends on.

### The gate stays inside

Verify is the sole merge gate; it already runs the same checks the hosted
workflows run. GitHub Actions keeps running on the replica as an advisory
drift detector: a red run means the replica's clean-room build disagrees with
the authority and someone should look, and it never blocks a land. Recording
this here keeps a future reader from re-deriving branch protection on the
replica out of habit.

## Consequences

- A hermetic end-to-end harness becomes an ordinary `cargo test`: a real
  coordinator over a temporary bare repository and a temporary work-order
  store, mock model lanes, real git. The executor's object handling — the
  #5025/#5027 class — becomes CI-catchable instead of production-discovered.
- The prompt-injection surface through work-order text closes: agents consume
  only signed, journaled work orders, never text that arrived from a public
  host. Publicizing the repository stops being gated on comment restrictions.
- Latency and availability decouple from a third party. Lane fetches and
  landings become local operations.
- GitHub's UI stops being authoritative for work-order state; the internal
  view and its projection are the record. The replica's issue and pull-request
  pages become read-only artifacts of the projection.
- Backup responsibility comes home. The replica keeps covering the source
  tree, but the journal and the work-order store need their own snapshot
  discipline on the fleet host.
- Serving the authority beyond one machine — additional lane hosts, or an
  operator working remotely — eventually means hosting the bare repository
  over the wire. That is follow-on infrastructure, not part of this decision.
- The work lands in two slices, each its own implementation issue: the source
  authority first, then the work-order store. This record flips to Accepted
  when both are implemented.

## Alternatives considered

- **Harden the inputs where they are** — signed markers inside GitHub issue
  bodies. Rejected: the signature would attest to text whose storage,
  history, and availability still belong to a third party, and the executor's
  testability problem would remain untouched.
- **An inbound contribution path** — external pull requests on the replica
  admitted as candidates at Verify under operator sign-off, riding the
  operator-repair machinery. Designed far enough to know it works, and
  rejected deliberately: the project has no external contributors to serve,
  the path would put foreign build scripts and tests into lanes on the fleet
  host, and the quarantine posture this record establishes is worth more than
  an unused front door. Recorded so a future reader knows the omission is a
  decision, not an oversight.
- **A self-hosted forge as the authority** — running a forge application and
  pointing the existing REST adapter at it. Rejected: the port needs an
  object store and a ref namespace, and a forge adds a web application, its
  accounts, and its upgrade cadence on top of the same git repository this
  decision uses directly.
- **Leaving GitHub authoritative and accepting the coupling.** Rejected by
  the forces above; kept here as the honest baseline. The replica retains
  what GitHub is genuinely good at — visibility, an off-host copy of the
  tree, clean-room build signal — without holding authority.
