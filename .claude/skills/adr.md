---
name: adr
description: Scaffold a new Architecture Decision Record from the template with the next sequential number. Invoke as `/adr <title>` (e.g. `/adr mail-first architecture`). Creates the branch and draft file but does not commit or open a PR — those happen after the content is written.
---

# ADR skill

Creates a new ADR in `docs/adr/` from `docs/adr/TEMPLATE.md`. Use when the user wants to record a load-bearing architectural decision.

## Procedure

1. **Confirm we're on a clean `main`.** If the working tree is dirty or we're on another branch with uncommitted work, stop and surface that to the user before continuing. `git fetch origin && git checkout main && git pull --ff-only`.

2. **Determine the next number.** Glob `docs/adr/*.md` for files matching `^(\d{4})-.*\.md$`. The new number is `max(existing) + 1`, zero-padded to 4 digits. If no numbered files exist, start at `0001`. Never reuse a number, even if a prior ADR was reverted.

3. **Slugify the title.** Lowercase. Replace whitespace runs with single hyphens. Strip characters other than `[a-z0-9-]`. Collapse repeated hyphens. Trim leading/trailing hyphens. Example: `Mail-first architecture` → `mail-first-architecture`.

4. **Create the branch.** `git checkout -b docs/adr-NNNN-slug` off the up-to-date `main`.

5. **Copy the template.** `cp docs/adr/TEMPLATE.md docs/adr/NNNN-slug.md`.

6. **Fill placeholders in the new file only:**
   - `ADR-NNNN: {{title}}` → `ADR-NNNN: <Title Case title as given by the user>`
   - `YYYY-MM-DD` → today's date in ISO format
   - Leave `Status: Proposed` (flip to `Accepted` at merge, or later).
   - Leave the Context / Decision / Consequences / Alternatives sections untouched — those are for the author to fill.

7. **Report back** the new file path and branch name. Ask the user for the substance. Do **not** commit, push, or open a PR yet — the ADR needs content first. Once the content is written, the normal PR flow applies (conventional PR title, e.g. `docs: add ADR-NNNN <title>`).

## Constraints and notes

- Never modify `docs/adr/TEMPLATE.md` or any existing ADR in this flow.
- The slug embedded in the filename must match the slugified title. If the user later renames the ADR, rename both the file and the branch.
- If the user invokes `/adr` with no title, ask for one before doing anything.
- If the working tree is dirty, stop and ask — don't stash or auto-commit.
