# ADR-0211: An Artifact Is a Record or It Is Rebuildable

- **Status:** Proposed
- **Date:** 2026-08-26

## Context

The bloomery leaves artifacts everywhere it works: session checkouts under
`sessions/<slug>/tree`, per-dispatch evidence directories (gate logs, lap
transcripts, identity files), candidate / integration / checkpoint refs in the
authority repository, the journal (`journal.sqlite`), the coordinator's
artifact store, and per-slot cargo target directories. Until now, what to keep
has been decided one janitor rule at a time, and two incidents showed the cost
of deciding it locally. On 2026-08-14 a budget sweep deleted a target
directory a live compile was standing in. On 2026-08-25 the janitor reclaimed
the session checkouts of members still walking; every later refine lap resumed
its session into a dead tree, produced a clean diff from the fresh checkout,
and falsely declined — board-5435 parked twice on phantom declines and was
withdrawn (dispatches 3301/3318). A third, quieter loss: candidate refs are
consumed at land, and the only reason one bloom's candidate set is still
recoverable is that an operator happened to copy the hashes into a scratch
file before withdrawal.

The operator's direction after the second incident sets the frame: the janitor
prunes only between blooms, or under disk pressure — and disk pressure means
pruning build directories, never source trees or text. Old trees are kept as
records of how the work was figured out. Session resuming is protected at all
costs.

The measured inventory makes the frame cheap to honor (2026-08-26):

| artifact | count / size | volume |
|---|---|---|
| session trees | 49 trees, ~1 GiB together | working |
| evidence directories | 437 dirs, few MiB each | working |
| journal | tens of MiB | working |
| coordinator artifact store | tens of MiB | working |
| lane slot targets | terabyte-scale | cache |
| stranded local slot targets | the bulk of working-volume pressure | working |

Everything anyone would keep is small. Everything large is a cargo target
directory, and a target directory is a cache. The disk incident that
motivated aggressive sweeping was `target/debug/incremental` — build state,
not records. The pressure on the working volume today is dominated by slot
targets stranded there when the target base moved to the cache volume, which
the janitor no longer looks at because its `target_base` points at the new
location.

## Decision

Every artifact the bloomery produces is classified once, here, into one of
three classes, and every reclaim rule derives from the class rather than from
a per-rule judgment call.

**Records are never deleted.** Session trees, evidence directories (the lap
transcripts inside them included), the journal, the coordinator artifact
store, and the daily branches are the archaeological record of how each
solution was found. No janitor rule, no disk-pressure response, and no
retention window deletes them. When a record has aged past its working
usefulness — its bloom terminal, its commission resolved — it may be
*archived*: moved or packed onto the archive tier, intact and listable, by an
explicit between-blooms action. Archival changes where a record lives, never
whether it exists. At today's sizes (a season of evidence directories is
single-digit gibibytes) archival is a tidiness measure, not a survival one.

**Working state is reclaimed between blooms.** Nonce-keyed dispatch checkouts,
terminal blooms' working refs, and the moved-aside leavings of past evictions
are consumed by the walk that made them. The janitor reclaims them only when
nothing walks: no bloom active and unlanded, no order outstanding — the gate
`sweep` (`crates/aether-chassis-bloomery/src/bloomery/reactor/janitor/sweep.rs`)
applies per the janitor-prune-scope commission. A session tree sits at the
protective end of this class: its lifetime is at least the resumability of its
session, which outlives any one bloom — a commission withdrawn from one bloom
and re-sealed into the next resumes the same conversation, and the resumed
session edits the tree it was born in. A session tree is therefore reclaimable
only when its commission is landed or cancelled, and even then it is archived
as a record rather than deleted, because the tree is the record the operator
named.

**Caches are the only disk-pressure kill.** Cargo target directories — the
slot targets under the configured `target_base`, and any stranded under an old
one — are rebuildable from source and the lockfile. Disk pressure evicts them
coldest-first back to the budget line (`sweep_targets`, with its per-directory
live-occupancy re-check), and nothing else: a pressure response that has
evicted every cold cache and is still over budget alerts the operator rather
than escalating up the ladder into working state or records. Candidate refs
stay working state, but their *hashes* are journaled at seal and at land, so
pruning a terminal bloom's refs can never again make a candidate
unreconstructable from the record.

The stranded slot targets are the first application: an explicit
between-blooms operator removal, clearing the bulk of the working volume's
pressure, after confirming per-directory that no lane configuration still
resolves them.

## Consequences

- The janitor's remaining judgment calls disappear: each sweep rule cites a
  class instead of arguing liveness case by case. The
  janitor-prune-scope regression scenario pins the between-blooms gate.
- Session resumption can no longer be invalidated by cleanup, because nothing
  that deletes runs while work walks, and session trees outlive their blooms
  by construction.
- Records grow without bound on paper. In practice they grow at megabytes per
  bloom; the archive tier absorbs years of that before the question returns,
  and if it returns the answer is a bigger disk or an explicit owner
  decision, never a janitor rule.
- Candidate-hash journaling is new follow-on work in the seal and land paths;
  until it lands, terminal-ref pruning keeps its current conservative
  successor-chain rule.
- The evidence retention window (`evidence_retention_days`) changes meaning
  from delete-after to archive-after.

## Alternatives considered

- **Retention windows that delete records.** Rejected: the record class exists
  precisely because "old enough to delete" was twice judged wrong by the
  system and overruled by the operator; the bytes saved are noise next to the
  target directories.
- **A liveness computation good enough to prune mid-walk.** Rejected: "no live
  member is bound" is a computed claim racing live work, and both incidents
  were that computation losing the race. Between-blooms is checkable from the
  same snapshot the sweep already replays.
- **One volume, one budget.** Rejected: the volumes already separate the
  classes physically — records are small and live with the working state,
  caches are huge and live apart — and a shared budget would let cache
  growth put pressure on records.
