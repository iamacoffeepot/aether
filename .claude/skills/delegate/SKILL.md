---
name: delegate
description: Dispatch a worktree-isolated Agent to implement a GitHub issue tagged `agent`. Invoke as `/delegate <number>` (e.g. `/delegate 317`). The issue body must carry a complete, scoped fix — the Agent reads it verbatim, branches, implements, runs CI checks, opens a PR. Use when an issue is small enough that the proposed fix is mechanical and you don't need to design anything in main session.
---

# Delegate skill

Dispatches a `general-purpose` Agent under `isolation: "worktree"` to ship a GitHub issue. The Agent's checkout lives in a temp worktree so it never collides with the main session's working tree.

## Procedure

1. **Confirm the issue is `agent`-eligible.** `gh issue view <NNN> --json number,title,labels,state,body` — verify `state == "open"` and `labels` contains `agent`. If not, stop and surface the gap (missing label, closed issue, no proposed fix in body). Do not auto-add the label.

   Also note whether `automerge` is in `labels`. If present, the user has pre-authorized auto-merge for this issue's PR; pass that signal into the Agent prompt below via `{{automerge_clause}}`. If absent, the Agent must NOT self-merge — the user reads and merges manually.

2. **Spawn the Agent** with `isolation: "worktree"` and `subagent_type: "general-purpose"`. The prompt is self-contained — the Agent has no prior conversation context, so everything it needs goes in the prompt body. Template:

   > Implement issue <NNN> in this aether repo: https://github.com/iamacoffeepot/aether/issues/<NNN>
   >
   > Read the issue body for the proposed fix — it carries the exact change. Apply it, keeping any contracts called out in the issue intact. If the body is ambiguous or missing the proposed-fix section, stop and post a triage comment on the issue (`gh issue comment <NNN>`) instead of guessing.
   >
   > Branch from fresh main as `<type>/<short-slug>` (type matches the Conventional Commit type the fix needs — `fix/`, `feat/`, `refactor/`, `docs/`). Run `cargo check --workspace --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt -- --check`, and `cargo test --workspace`. If the change touches a wasm component, also run `cargo check -p <crate> --target wasm32-unknown-unknown`.
   >
   > Commit with a Conventional Commits subject (`type(scope): description (issue <NNN>)`, lowercase subject — CI lints PR titles, and main squash-merges the PR title as the commit subject). Push the branch. Open a PR with the same Conventional Commits title and a body including `Closes #<NNN>` (the `#` is required — `Closes issue <NNN>` does NOT trigger GitHub auto-close) plus a 1–3 bullet summary. Hand back the PR URL in the final report.
   >
   > {{automerge_clause}}
   >
   > Constraints: no destructive git ops, no `--no-verify`, no force-push, do not amend an existing commit.

   Fill `{{automerge_clause}}` based on step 1:
   - **`automerge` label present:** `After the PR is created, run \`gh pr merge <PR-number> --auto --squash --delete-branch\` to queue auto-merge once CI is green and required reviews are satisfied. The user has pre-authorized this via the \`automerge\` label on the issue.`
   - **`automerge` label absent:** `Do not self-merge — the user reviews code PRs before merge.`

3. **Report back** with the Agent's PR URL (or its triage-comment URL if it bailed on ambiguity). Note any check failures or skipped steps, and whether auto-merge was queued, so the user knows what state the branch is in.

## Constraints and notes

- The `agent` label is the gate, not a suggestion. If the issue isn't tagged, the user opted not to make it agent-ready; respect that.
- If the proposed fix in the body looks design-bearing (architectural decisions, new public APIs, ADR-worthy choices), bail and post a triage comment. The skill is for mechanical fixes, not design.
- One PR per invocation. If the user wants multiple issues delegated, they invoke the skill multiple times.
- The `automerge` label is the per-issue authorization signal. It is independent of `agent` (and orthogonal to the docs-only self-merge default): without it, the Agent never queues auto-merge regardless of CI state.
- Use `/sweep` after delegated PRs land to clean up the worktrees + branches the Agent created. The Agent does not delete its own worktree, only the remote branch (via `--delete-branch` when auto-merge is enabled).
- The Agent will not have access to the live MCP harness (different session). Smoke testing is opt-in: if the Agent's change is render- or substrate-touching, it should explicitly note in the PR body that live verification is pending. Unit tests + clippy cover most cases.
- If the Agent's CI checks fail and the cause isn't obvious, it should push the branch + open the PR anyway with the failure noted in the body — the user reviews and decides whether to retry or take over manually.
