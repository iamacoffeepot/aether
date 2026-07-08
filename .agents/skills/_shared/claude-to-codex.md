# Claude-to-Codex Workflow Translation

Use this reference when a Codex repo skill points at an existing `.claude` skill or workflow file.

## Required Read Order

1. Read the Codex skill `SKILL.md`.
2. Read this translation reference.
3. Read the referenced `.claude/skills/<name>/SKILL.md` or `.claude/workflows/<name>.js` completely before acting.

## Translation Rules

- Treat the referenced Claude artifact as source material, not executable Codex code.
- Preserve the repository's phase-label lifecycle: Backlog has no `phase:*`, scoped issues stop at `phase:plan`, approved issues move to `phase:ready`, implementation moves through `phase:executing` and `phase:refine`, and Done is a closed issue.
- Prefer `gh api` REST forms for GitHub operations whenever a REST endpoint exists. Avoid GraphQL-backed `gh issue create`, `gh issue edit`, `gh pr list`, and `gh pr checks` where the Claude workflow names a REST alternative.
- Map Claude slash commands to Codex skills. For example, `/scope` means use the `scope` skill, and `/implement` means use the `implement` skill.
- Map Claude `agent(...)` workflow calls to Codex subagents only when the active skill explicitly asks for subagents or the user explicitly requests delegation.
- Preserve freshness boundaries. If a Claude workflow says a child agent must not see a diff or implementation detail, spawn a fresh Codex subagent without forked context and pass only the allowed prompt.
- Map Claude `phase(...)` and `log(...)` calls to concise progress updates in the main Codex thread.
- Map Claude JSON schemas to required structured return shapes in prose. Validate the returned shape before rolling it up.
- Map Claude worktree isolation to git worktrees under `.claude/worktrees/issue-<N>` unless the skill explicitly says a temp scratch directory is enough.
- Do not edit `.claude/` artifacts while executing a Codex port unless the user explicitly asks to change the Claude workflow itself.
- Do not add Codex hooks as part of a skill run unless the issue being implemented explicitly scopes hook work. Hook semantics and trust review differ between Claude and Codex.

## MCP Notes

The repo's `.mcp.json` names the live engine server `aether-hub`. Codex may expose the same tool family as `mcp__aether-hub__*` when the MCP server is configured and connected. If live engine tools are absent, start `scripts/ensure-tunnel.sh` only when the task requires them, then reconnect MCP in the active Codex surface.
