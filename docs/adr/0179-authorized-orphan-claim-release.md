# ADR-0179: Authorized orphan claim release

- **Status:** Accepted
- **Date:** 2026-08-10

## Context

Bloomery coordinates claims across independent instances by publishing typed Git refs. A ref can outlive the journal that created it. When every surviving journal lacks the holder, boot reconciliation deliberately treats it as foreign and report-only; supersession also cannot act because its predecessor must exist locally. One orphaned mainline-admission ref can therefore refuse every future seal on the shared remote.

ADR-0150's conservative rule is correct: absence from one instance's journal is not proof that a foreign holder is dead. Automatically releasing every locally unknown ref would destroy legitimate work owned by another instance. The missing capability is an attributable operator decision to accept that uncertainty, constrained to one typed ref and one expected holder and executed with the source port's existing compare-and-swap release.

A direct REST-to-Git deletion would bypass journal truth and lose auditability across crashes. A durable request must also distinguish a concurrent holder change, an already absent ref, a locally known holder, and a successful release, and must make retries safe.

## Decision

### Typed authorization

Introduce `OrphanClaimRelease { ref_kind: ClaimRefKind, expected_holder: BloomId }` as the complete mutation target. No API accepts a raw ref path. Its content digest is the request id.

The operator supplies an author-signed `Statement` whose parents include that request digest and whose words are exactly `release orphan bloomery claim`. The host verifies the signature through the configured signing capability before admission. The reducer independently requires instruction-capable author provenance, the exact words, and the matching parent. The statement authorizes acting under uncertainty; it does not claim that local absence proves death.

### Durable lifecycle

Append a request fact and a completion fact. The request fact is accepted only while no `BloomRecord` for `expected_holder` exists in the local snapshot. A known active, resolved, landed, or superseded holder is refused; existing reconcile and supersede paths remain responsible for known records.

An accepted request records a pending release keyed by request digest and emits a transactional-outbox effect carrying `CompleteRelease(Some(expected_holder), ref_kind)`. A focused reactor executes the existing source-port compare-and-swap operation and admits one completion.

The per-ref release operation therefore reports its own result type rather than the `ClaimOutcome` the seal / transfer / release-seal operations share. Those three are all-or-nothing over a ref set, where "every ref reached the target state" is the whole answer, so `Acquired` collapses a live expected-holder deletion, a tombstone sweep, and an already-absent ref into one value. A durable release has to journal which of them happened, so `complete_release` returns `ClaimReleaseOutcome::{Released, AlreadyAbsent, Changed { observed_holder }}` and its transact-mail replies `aether.source.complete_release_result` instead of the shared `aether.source.claim_result`. Boot reconciliation treats every clean variant as converged; only a source fault remains an error. The three completions are:

- `Released` when the expected holder was deleted;
- `AlreadyAbsent` when the typed ref no longer exists;
- `Changed { observed_holder }` when the ref exists under another holder;
- `SourceFault` only for an operational failure that remains retryable and does not complete the request.

`Released`, `AlreadyAbsent`, and `Changed` are terminal journaled results. `Changed` never retries against the new holder. Replaying the request id returns the recorded pending or terminal state and emits no second release effect. If the source mutation succeeds and the process dies before completion admission, redriving sees `AlreadyAbsent` and completes idempotently.

### Operator surface

`GET /claims` enumerates typed claim refs and holders through the existing guarded source mail. `POST /claims/releases` accepts the typed request and signed statement, returning `202` with the request digest once the request fact is admitted. `GET /claims/releases/{digest}` returns pending or the terminal result from journal-derived state. Invalid signatures, locally known holders, malformed kinds, and authorization mismatches are synchronous refusals; no mutation is attempted.

Enumeration is diagnostic, not a liveness oracle. The operator must investigate the holder before signing. This decision adds no heartbeat, lease, age-based eviction, or automatic foreign-claim release.

## Consequences

- An orphan can be inspected and retired without out-of-band `git push --delete`, while a concurrently changed ref remains protected by the expected-holder CAS.
- Every attempted mutation is bound to a typed target, an author signature, a request digest, and a journaled terminal result.
- A locally known holder cannot use this escape hatch; normal lifecycle recovery remains authoritative.
- `AlreadyAbsent` is an idempotent terminal success, including the crash window after source deletion and before completion admission.
- The REST API, reducer fact/decision vocabulary, snapshot projection, outbox payload, source reactor, and guide gain one coordinated pre-1.0 surface.
- Automatic orphan detection remains intentionally unsolved. A future lease or heartbeat protocol requires a separate ADR.

## Alternatives considered

- **Release every locally unknown holder** — legitimate foreign blooms are also locally unknown.
- **Accept a raw ref path** — it escapes the typed namespace and can target unrelated Git refs.
- **Delete directly in the API handler** — it bypasses journal ordering, transactional outbox redrive, and durable audit state.
- **Require absence to be an error** — a crash after successful deletion would make the same authorized request impossible to complete idempotently.
- **Expire by age or add a heartbeat here** — age is not liveness, and leases require a broader cross-instance coordination protocol.
