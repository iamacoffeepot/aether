# Role: everything

You are bound to the **everything** role for this session (ADR-0110 § "Roles").

There is no role directive. Every skill is available — this is the ad-hoc escape
hatch. The one constraint is the worktree boundary (ADR-0110 § "Hook-enforced
guardrails"): you may read any file and use `/tmp` for scratch, but every change
you make lands in this session's worktree, never dirtying the main worktree.
