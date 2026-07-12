---
name: adr
description: "Scaffold a numbered Aether Architecture Decision Record in a dedicated Codex worktree. Use when a load-bearing decision needs a Proposed draft from docs/adr/TEMPLATE.md; do not use for ordinary implementation notes."
---

# ADR

Read [Codex harness](../_shared/codex-harness.md). Require a non-empty title. If none was supplied, end the turn by asking for it and make no changes.

1. Resolve `main_root` from the absolute common git directory and fetch `origin main`. Never check out or modify the primary `main` worktree.
2. Enumerate numbered `docs/adr/NNNN-*.md` paths across `origin/main`, local/remote refs, and registered worktrees. Choose one greater than the maximum observed number; never reuse a number merely because a prior ADR was reverted or is not on main.
3. Slugify the title: lowercase; whitespace to one dash; remove characters outside `[a-z0-9-]`; collapse and trim dashes. Refuse an empty result.
4. Set:

   ```text
   branch = docs/adr-NNNN-<slug>
   worktree = $main_root/.agents/worktrees/adr-NNNN-<slug>
   file = <worktree>/docs/adr/NNNN-<slug>.md
   ```

5. Refuse an existing path, branch, or ADR number collision. Create the dedicated worktree and branch from `origin/main`.
6. Read `docs/adr/TEMPLATE.md` in that worktree and create the new file with `apply_patch`. Make only these template substitutions:
   - `ADR-NNNN: {{title}}` → `ADR-NNNN: <user title>`;
   - the status choice line → `- **Status:** Proposed`;
   - `YYYY-MM-DD` → today's local date in ISO form.
7. Preserve the Context, Decision, Consequences, and Alternatives prompts unchanged.
8. Verify the template and every existing ADR are untouched. Report branch, worktree, and file, then ask the user for the decision substance.

Do not commit, push, open a PR, accept the ADR, or author a decision the user has not supplied. Once the content is complete, use normal Conventional Commit PR handling.
