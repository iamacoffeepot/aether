# Amending a member's declared surface

A member of a bloom that cannot finish inside its declared surface stops and
says so. Since ADR-0207 it stops with the paths named: the lane writes a
`.bloomery-surface-request` deliverable, the coordinator journals it as a fact,
and the member renders on `/view` with an `awaiting_surface` block listing the
paths and the one line justifying each. Nothing further dispatches against it —
another lap would reproduce the same refusal — so the member waits on a person.

Answering that request is `cargo xtask bloom amend`.

## Why this is not a flag flip

The declared surface is a field of an immutable, content-addressed
`ScopeRevision`, and the approval statement's signed words *are* that revision's
digest. There is no widening in place. Giving the member one more path means:

1. writing a successor revision whose surface is the union,
2. producing an approval bound to that new revision's digest, and
3. re-sealing the whole membership as a successor bloom.

Two things follow from that shape, and the command's output states both.

**Writing the revision commits you to finishing.** `POST
/commissions/{id}/revisions` advances the commission's current revision
immediately. Between that write and the approval, the member is unsealable —
the seal door refuses a member whose tip carries no approval — and any
supersede that still names the old digest is refused as stale. The command
announces the write before it makes it, and every step after it is
check-then-act over a deterministic signature, so re-running the identical
command after a failure converges rather than compounding. A re-run reads its
own handiwork: a tip already declaring the surface the amendment would write is
approved and sealed where it stands, which is how a run that failed downstream
of the write gets finished rather than blamed on a human who moved the scope.

**The amended member restarts.** A successor member carries no stage cursor, so
the amended lane re-enters at `Construct`. The park's progress is the price of
the wider surface.

## The tier ladder decides who may grant it

This is the load-bearing half. The seal door verifies that an approval's
signature is by an allowlisted key — not that the *tier* permitted that key.
The operator key is on the coordinator's allowlist, so an operator-signed
approval over a widening that resolves `human` is admitted exactly as an `auto`
one is. Human tier, downstream of a signature, is enforced by the human
declining to sign.

`amend` is the one point in the chain that can refuse *before* a signature
exists, so it is where the ladder is applied to the delta. It resolves the
policy the successor's seal will actually gate against — the predecessor
bloom's sealed `aether.bloomery.approval_policy` when it sealed one, read back
through `GET /configs/{digest}`; the repository `approval-policy.toml` only
when it sealed none. A sealed policy that cannot be read or parsed is a
refusal, never a fallback: a verdict against the wrong policy reads as
authoritative and is worse than no verdict.

`--accept-tier` is the ceiling the amendment may grant unattended (`auto` by
default). A widening above it is refused with each offending path and the tier
it resolved, and the member stays parked with its request intact.

## Driving it

```bash
cargo xtask bloom amend <bloom-id> \
  --workpiece issue-5321 \
  --task-file work-order.md \
  --seed-file ~/.aether/operator.seed \
  --accept-tier auto \
  --dry-run
```

`--dry-run` runs every read — the request, the commission tips, the policy, the
granularity each path is admitted at, the tier verdict, the sibling overlaps the
widening creates — and writes nothing. It prints the same table a refusal
carries, so the operator sees which path costs them the amendment before
spending anything.

A blocked lane names the file it stopped on, and the seal door admits a
declared-surface entry naming one file only when a file-granular
approval-policy rule names that same file. So the amendment widens the ask to
the glob covering it — `crates/<crate>/**`, `docs/<book>/**`,
`.github/<area>/**`, or the top-level tree anything else lives in — and prints
the ask beside the grant. The same rewrite runs over the entries the current
revision carries, so a raw file entry sealed before the request arrived leaves
with it.

Drop `--dry-run` to grant it. `--path` unions extra globs into the lane's
request, and is required when the member carries no request at all — amending a
member that asked for nothing, on the strength of nothing, is what turns a
declared boundary into a suggestion.

The signing seed is 32 raw bytes or 64 hex characters, mode `0600`; a seed any
other account on the host can read is refused. The coordinator holds no private
keys (ADR-0149 §The boundary), so every approval at every tier is signed here.

## What it checks before it writes anything

| Check | Refusal |
|---|---|
| The bloom, the member, and its `awaiting_surface` | no such bloom or member; the member itself was withdrawn; no request and no `--path` |
| Every requested path is inside the declared-surface grammar | names the first glob outside it — the lane is untrusted, and a glob would widen the appeal past the refusal that prompted it |
| The request names the revision the bloom sealed the member at | a human moved the scope; re-scope rather than amend |
| The commission tip is the sealed revision, or already carries the surface this amendment would write | a human moved the scope; re-scope rather than amend |
| Every *standing sibling* is at its own commission tip and carries an approval | refused now, because the successor seal would refuse after the key had signed. A withdrawn member is no sibling here: it never integrates, so its commission is free to have been re-scoped into a later bloom, and the successor leaves it out |
| Every path is at a granularity the seal admits | a path naming one file no approval-policy rule names is widened to the glob covering it — its crate, or the tree it lives in — and both spellings are printed. A repository-root file the policy does not name has no tree to widen to, and is refused in the same words the seal door uses |
| The additions are already covered | granted anyway: the tip is approved and sealed as it stands, so a run after a downstream failure finishes the amendment instead of declining it |
| The policy the successor's seal will use | a sealed policy that will not read or parse |
| The tier gate over the delta | above `--accept-tier`, naming each offending path |
| Sibling surface overlaps the widening creates | advisory — the seal journals them and may derive a dependency edge |
