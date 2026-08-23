# ADR-0203: Main Advances in the Fleet Repository

- **Status:** Accepted (ratified by owner 2026-08-19; implementation landed via #5173; the advance amended 2026-08-21 for #5414)
- **Date:** 2026-08-19

## Context

ADR-0199 moved source authority to the fleet repository and demoted GitHub to a one-way replica — for the daily refs. The mainline advance kept its earlier shape: ADR-0186's sync-back linearizes the day onto main through a GitHub pull request gated on the full required CI, merged by rebase-merge. That was the right gate when GitHub was the authority; after the flip it is the last place GitHub still decides anything, and it reintroduces exactly the dependencies the flip removed — a network round-trip in the day's critical path, an OAuth credential surface (the replica-push class of #5169), and a required-check gate whose content the verify lane already runs locally (the six-gate equivalence #4883 tracks). The 2026-08-18 roll demonstrated the cost concretely: a mid-day direct-to-main merge (#5162) conflicted with the day image, and the refusal was healed by hand through a manually merged pull request (#5170).

The local gate the sync-back actually needs already exists: the verification ledger (ADR-0200) records per-bloom proof facts, and the roll barrier refuses to advance unless the day's coverage map is fully green (#5131).

## Decision

The day returns to main inside the fleet repository; GitHub carries no gates.

- **The advance.** The roll writes the day's tree to `refs/heads/main` in the fleet repository under the same compare-and-swap discipline as a landing. No pull request, no merge ceremony. The image is *constructed* — one sync commit carrying the day's tree, whose only parent is current main — rather than replayed commit by commit: construction satisfies the barrier's three properties (linear, no merge commit, byte-identical tree) outright and cannot conflict, and it is what the roll must be able to do unattended. The ADR-0186 authored-commit replay stays available behind an opt-in flag for a day with no folds in it, and only there — a day carrying bloom fold merges cannot be replayed linearly at all, because an early commit whose hunks a later one rewrote is not "already upstream" by patch id and `git rebase` stops on it. Because construction cannot conflict, it also cannot notice a day that has fallen behind main, so the advance asks for that ancestry explicitly and refuses without it.
- **The gate.** The advance is barred by the day's verification-ledger coverage map being fully green (ADR-0200; the roll barrier of #5131). The ADR-0186 invariant is unchanged — main receives only fully-proven trees — only the prover moves, from GitHub required checks to the ledger that already records the same gates per bloom.
- **The replica.** After the advance, main is pushed to GitHub the same one-way, best-effort way as the daily refs (ADR-0199). GitHub CI on the replica is an advisory drift detector: a red run is an operator signal, handled by the ADR-0186 backstop reaction (quarantine and repair bloom), never a gate.
- **Conflicts heal locally.** A conflict on the opt-in replay path (the #5170 class) is resolved in a fleet worktree and advanced through the same compare-and-swap, or the operator drops the flag and syncs the tree; there is no pull-request escape hatch to fall back to. The class itself shrinks structurally: with no inbound GitHub path, nothing lands on main except the roll, so the mid-day-divergence that caused the conflict cannot recur.

## Consequences

- The last GitHub authority over the tree is gone. GitHub's remaining roles are replica, advisory CI signal, and Pages hosting for the guide — all outputs.
- The rebase-merge carve-out in ADR-0186 is retired, and GitHub branch protection on main stops guarding anything; its required checks must be removed so the mirror push cannot be refused by an advisory signal.
- Direct GitHub-side commits to main stop being an input path; the mirror push refuses drift instead of merging it, which deletes the #5162 conflict class rather than automating its repair.
- `xtask bloom roll`'s sync half rewires from pull-request machinery to the local advance; the cut half now cuts tomorrow's branch from fleet main rather than a GitHub fetch. Every `git` the roll issues is rooted at the fleet repository rather than the process cwd, so the roll runs from anywhere.
- The day's authored commits do not reach main by default. Main carries one sync commit per day, and the per-bloom history stays readable on the day branch, which is retained.
- Amends ADR-0186 (the sync-back bullet and the merge ceremony); extends ADR-0199's replica posture to the mainline ref.

## Alternatives considered

- Keep the GitHub pull-request gate: rejected — its check content is equivalent to the local verify arms (owner ruled it cosmetic, 2026-08-15), and it retains an OAuth and network dependency in the roll's critical path.
- Gate the advance on a fresh local full-suite run instead of the ledger: rejected — it re-proves what the ledger already recorded per bloom; the eager async backstop remains the detector for scoping misses.
