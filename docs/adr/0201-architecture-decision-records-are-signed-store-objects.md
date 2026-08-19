# ADR-0201: Architecture Decision Records Are Signed Store Objects

- **Status:** Proposed
- **Date:** 2026-08-18

## Context

ADR-0199 moved work-order authority off GitHub issue bodies and into signed
commissions. Architecture decision records are still the earlier shape of the
same problem: status is a markdown line anyone can edit, acceptance is
convention, supersession is prose, and the accepted-only-after-implementation
rule is agent discipline with no machine trace.

`docs/adr/` is a good reading surface. It is a poor authority. A hidden
approval marker taught that lesson for commissions; a `**Status:** Accepted`
line is the same class of unsigned claim.

#5045 froze a version-1 `ScopeRevision` encoding. A commission that
implements an ADR needs to name that ADR by digest inside those bytes, which
is why this record was coordinated against that schema before production
signatures covered it.

## Decision

Architecture decision records become first-class objects in the bloomery
store, beside commissions, in the same SQLite database and migration
versioning.

The canonical value is a versioned `Adr`: number, title, date, and the
template sections (context, decision, consequences, alternatives). Canonical
wire bytes are the identity. The digest is recomputed on read. Markdown is
rendered from the value and is never byte-preserved. Schema version sits in
the bytes from the first row.

Status is an append-only transition log, not a column on the ADR row:

- **Proposed** registers the ADR. Unsigned.
- **Provisional** is the machine's statement that work may proceed pending
  the owner's batched daily read. Unsigned; a provisional record must not
  carry a signature.
- **Accepted** is an owner-signed statement over the ADR digest through a
  new `AuthorityDoor::Accept` (ADR-0182). A ratification record must carry a
  signature. The record may cite resolution or evidence digests that
  discharged the implementation checklist; empty citations are legal
  (docs-only ADRs) and the field exists from row one.
- **Superseded** names the successor's digest. It consumes Provisional the
  same way ratification does. Revocation of an already-accepted ADR is not
  this decision.

No unsigned status column is ever authoritative. The last transition is the
only status a reader may believe.

A commission's version-1 `ScopeRevision` carries a trailing list of ADR
digests it implements, empty when the commission binds no stored decision.

`docs/adr/NNNN-title.md` becomes a rendered mirror of the canonical value,
byte-stable for unchanged input, so the file tree can be checked against the
store the same way the source mirror is. Until a later cutover, a PR against
the mirror remains the human reading surface. Existing markdown ADRs are not
migrated by this decision.

## Consequences

- Acceptance is a signature the machine verifies, not a line an editor can
  type. The accepted-after-implementation rule becomes a query over cited
  digests rather than a remembered convention.
- Work may proceed under a Provisional record before the owner ratifies, and
  the daily roll can require that no provisional record is left unratified
  before main receives the day's tree.
- The `docs/adr/` tree stops being the source of truth once the store is in
  use. Reviewers still read markdown; they no longer edit authority by
  editing status.
- Scope-revision signatures cover the implemented-ADR list. A commission
  cannot silently drop or add a binding after approval.
- Migrating the existing ~200 markdown ADRs into rows is follow-on work.
  Until that lands, the store and the historical files coexist.

## Alternatives considered

- **Keep markdown authoritative and sign the files.** Rejected: the
  signature would attest to text whose status line can still be edited
  independently of the signed bytes, repeating the commission-marker
  failure.
- **Put status on the `Adr` value itself.** Rejected: Proposed → Accepted
  would change the digest an Accept signature covers.
- **Skip Provisional and go Proposed → Accepted.** Rejected by the owner's
  batched-ratification direction: the machine needs an unsigned "work may
  proceed" record that ratification and supersession both consume.
- **A new store, separate from commissions.** Rejected: the encoding,
  migration, and statement machinery already live on this connection; a
  second path would fork authority.
