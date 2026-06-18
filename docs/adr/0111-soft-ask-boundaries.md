# ADR-0111: Soft, ask-to-confirm role and worktree boundaries

- **Status:** Superseded — follows ADR-0110. The role boundary was removed with the role model; the ask-to-confirm worktree boundary it describes is kept, now keyed off the session's worktree rather than a role marker.
- **Date:** 2026-06-14

## Context

ADR-0110 binds each session to a role and enforces two boundaries with a `PreToolUse` hook that hard-denies (`exit 2`): the **role boundary** (a dreamer/scoper may not approve/implement/merge/push; an orchestrator may not wish/sketch/create issues) and the **don't-dirty-main worktree boundary** (an edit landing in the main checkout is blocked before it runs). The stance there was "enforcement is the payoff" — a hard block is what removes shared-clone collisions and bounds each session's blast radius.

The role system is still being shaped, and in practice the hard wall is too rigid for that stage. A hard deny:

- forecloses a legitimate one-off cross-role action, forcing the operator to re-declare the session's role or hand the step back;
- over-blocks paths that do not actually dirty aether-main — a write into a nested independent git repo or a wholesale-gitignored directory (the case #1841 reports);
- gives no in-the-moment escape hatch, unlike the guardrail-self-modification flow, which surfaces the action and asks for an explicit sign-off rather than refusing it.

## Decision

Replace the hard deny with an **ask-to-confirm** gate. When a session crosses a boundary, the `PreToolUse` hook returns `permissionDecision: "ask"` (exit 0, via `hookSpecificOutput`) instead of `exit 2`, surfacing a confirm prompt carrying the same reason text the deny used to print. The operator approves or declines in the moment; nothing is silently walled off. This applies to **both** boundaries — role and don't-dirty-main — through the one mechanism.

The boundary becomes a deliberate-friction gate rather than a wall: crossing it stays possible but is never accidental, because it always stops for an explicit yes. The invariant the worktree boundary protects — a clean main checkout — is still protected: a main-dirtying write must be actively confirmed, not merely warned about.

Two mechanism details follow from how the harness exposes each interception point:

- **Skill invocations are advisory, not ask.** The `UserPromptExpansion` event (where a slash-command/skill expands) can warn-or-block but has no native "ask", so an out-of-role skill surfaces an advisory note and proceeds. Its dangerous *effects* — merge, push, issue creation, a main-dirtying write — still hit the `PreToolUse` ask-gate, so the effect-level guard is unchanged. This makes a dedicated skill-invocation block (the earlier plan for #1837) largely redundant; it drops to an optional advisory.
- **The PostToolUse don't-dirty-main tripwire is unchanged.** A Bash command's effect is open-ended and can only be detected after it runs; the tripwire already reports-and-suggests-revert rather than preventing, which is the soft posture by construction.

Fail-open on an absent or empty role marker is preserved: an unbound session is never gated.

## Consequences

- Each boundary crossing becomes a confirm prompt instead of a refusal. The operator gains an in-the-moment escape hatch; the cost is a prompt per crossing, mitigated by allowing aether-gitignored paths silently so routine scratch (research/, worktrees) never prompts (#1841).
- The ADR-0110 guarantee weakens from "the crossing is impossible" to "the crossing is consciously confirmed." For concurrent-orchestrator sharding this means a main-dirtying write prompts rather than being walled off, so a sharded operator must not blind-approve. Acceptable while the system is being shaped; revisit if blind-approve proves to be a real failure mode.
- Realization is a small change to the merged `.claude/hooks/check-role-boundary.sh` (the `exit 2` blocks become the ask JSON) — guardrail self-modification, landing only under explicit authorization. Tracked by #1841 (broadened to the full soft-gate conversion).
- Supersedes ADR-0110 § "Hook-enforced guardrails" for the enforcement *action* (deny → ask). The boundaries themselves, their scope, the worktree binding, and the sharding model all stand.

## Alternatives considered

- **Advisory warn-only (no prompt)** — proceed with a note the session can ignore. Lowest friction but weakest guard: an accidental cross-role merge would still go through. Rejected — the confirm stop is exactly what keeps a crossing from being accidental.
- **Hybrid (ask for high-cost actions, warn for low-cost ones)** — finer-grained, but adds a per-action cost classification to carry in the hook for little gain while the role set is small. Rejected for a uniform ask.
- **Soften only the role boundary; keep don't-dirty-main a hard block** — the worktree guard protects a concrete past failure (shared-clone collisions between concurrent sessions). Rejected — an ask still protects the invariant via active confirmation while removing the wall, and one uniform mechanism is simpler than two.
