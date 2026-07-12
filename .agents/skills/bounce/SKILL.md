---
name: bounce
description: "Regress an Aether issue to Define, Design, or Plan with a recorded reason. Use when scope, approval, implementation, or review proves an earlier phase must be redone; do not use for environment outages."
---

# Bounce

Read [Codex harness](../_shared/codex-harness.md) and [GitHub workflow](../_shared/github-workflow.md).

Require an issue number, target `define|design|plan`, and a non-empty reason. Preserve the user's reason verbatim as markdown data, never shell input.

Read the issue and derive its canonical phase. Validate:

- target is Define, Design, or Plan;
- issue is open and not already bounced;
- current state has exactly one phase;
- target is strictly earlier under `Backlog < Define < Design < Plan < Ready < Building < QA < Findings < Held < Done`.

List every refusal. A same-phase target is a no-op, a later target is advancement, and a closed issue needs a new issue rather than a bounce.

On pass:

1. Re-read identity and labels.
2. Atomically preserve labels other than `phase:*` and `bounce-to:*`, then append exactly `phase:bounced` and `bounce-to:<target>`.
3. Stage and post one comment:

   ```markdown
   **Bounced to <Target>** (from <Previous>)

   <reason>
   ```

4. If the label write succeeds but the comment fails, retry only the comment. Never edit the issue body.
5. Report the transition and `Next: $scope <N>`.

The post-Ready states are reconciler-owned and are never bounce targets. Treat a retired `phase:executing` or `phase:refine` straggler as post-Ready for this earlier-than check.

Other skills use the same self-bounce mechanics. A design discovery targets Design; an incomplete/stale plan or exhausted retry budget targets Plan; an unframable issue targets Define. An environment, authentication, runner, or network outage uses `phase:stalled` with no bounce label instead.

On resume, `$scope` consumes the `bounce-to:*` label, removes every bounce label, restores the target phase, and reruns that phase plus all downstream phases.
