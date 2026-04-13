# ADR-0001: Record architecture decisions

- **Status:** Accepted
- **Date:** 2026-04-13

## Context

Aether will accumulate architectural decisions over time — renderer choice, scripting substrate, component model, scheduling, Claude integration shape, and more. Some are load-bearing enough that future contributors (human or Claude) will need to understand not just *what* was chosen but *why*, and under what constraints.

Chat is too ephemeral to serve as the record. Issues work for tracked work but aren't indexed for decisions. Commit messages are too terse. We need a dedicated, versioned, reviewable home for decisions.

## Decision

We use Architecture Decision Records (ADRs), stored as markdown files in `docs/adr/NNNN-title.md` with sequential numbering. Each ADR captures context, the decision, and consequences. ADRs are reviewed via PR like any other change, so the decision itself is subject to the same review bar as code.

`docs/adr/TEMPLATE.md` is the starting point for new ADRs.

## Consequences

- Decisions are grep-able, linkable from PRs and issues, and versioned alongside the code they govern.
- When a decision is revisited, the old ADR stays in history with its status updated to `Superseded by ADR-XXXX`. No rewriting past reasoning.
- There is a small discipline cost per decision. We mitigate this by only requiring ADRs for *load-bearing* architectural choices — not every design detail. Target order of magnitude: ~10s of ADRs over the project's life, not 100s.
- The workflow complements, rather than replaces, chat discussion and issues:
  - **Chat**: exploration, pressure-testing, fast dialogue.
  - **Issues**: planned work, open investigations, spike tasks.
  - **ADRs**: the decisions that survive.

## Alternatives considered

- **Issues-only** — decisions buried in closed-issue threads. Hard to find, hard to distill the conclusion from the dialogue.
- **A single `DECISIONS.md` log** — works at small scale, becomes a merge-conflict and organization nightmare past a dozen entries.
- **Wiki** — lives outside the repo, drifts from code, not part of the review flow.
