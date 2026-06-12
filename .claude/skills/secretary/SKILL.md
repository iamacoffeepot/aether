---
name: secretary
description: Surface everything that needs your attention as a ranked queue, then prompt with `AskUserQuestion` selectors so resolution is click-driven instead of typed. Bidirectional — invoke as `/secretary` to see what's pending, or have Claude/agents post a blocker via `/secretary --post-blocker` when mid-task work needs a user decision. Scans observable state (open PRs, project board phases, failing CI, unresolved bounce reasons) plus a posted-blockers file. Output is a printed queue followed by selector questions (HIGH/MED) — LOW items print-only.
---

# /secretary — pending-attention queue

`/secretary` is a scan-and-report skill. It tells the user what's pending their attention, grouped by priority, with each item shaped as either:

- **Document** — read this thing (PR / issue / wish report) and run a follow-up command.
- **Multi-choice** — pick one option from a short list to unblock work.

Bidirectional: the user invokes `/secretary` to see what's queued; Claude or any agent invokes `/secretary --post-blocker` when it's stuck mid-task and needs a user decision to continue. Posted blockers join the queue automatically.

Resolution is selector-driven. After printing the queue, Claude calls `AskUserQuestion` with one question per actionable HIGH/MED item — the user clicks options instead of typing replies. Inline-text resolution still works as a fallback (*"approve #973 and #974, bounce #984 with reason X, save memories 1 and 3"*) and for items that need free-form input the selector's automatic "Other" option captures it.

## Invocation

```
/secretary                       scan everything, print the ranked queue
/secretary --high                only HIGH priority items
/secretary --since <date>        items newer than <date>
/secretary --post-blocker        (claude/agent-side) record a blocker for user
   --type <choice|document|memory|review>
   --title "<short title>"
   --description "<longer body>"
   --options "a) ... | b) ... | c) ..."     # for type=choice; pipe-separated
   --source "<who posted: agent-id, skill-name, or session-id>"
/secretary --clear-resolved      garbage-collect resolved blockers from the file
```

## Sources scanned (observable state)

Most blockers are observable; the skill doesn't need anything written ahead of time:

1. **Open PRs you authored** — run `scripts/wave-status.sh` once to get the whole sweep: one line per open PR with CI verdict, draft state, review state, and head branch (REST only — no `gh pr list` / `gh pr checks`). Read the output directly; no per-PR enrichment loop needed. Items with `ci:failure` or `review:changes-requested` are HIGH; items with `review:none` and created more than 24h ago are MED.
2. **Open PRs from agents on your behalf** — `gh api -X GET search/issues -f q='repo:iamacoffeepot/aether is:pr is:open author:app/*' --jq '.items[].number'` (REST search — its own pool, separate from core REST and GraphQL; or filter source 1's list by branch prefix if agents use a convention).
3. **Active release project** (if `.claude/release-state.json` exists) — items at `Phase=Plan` with `AgentReady=No` (awaiting `/approve`), `Phase=Bounced` (need triage), `Phase=Stalled` (env/tooling).
4. **Issues labeled `phase:bounced` / `phase:stalled`** — `gh api 'repos/iamacoffeepot/aether/issues?labels=phase:bounced&state=open' --jq '.[].number'` (one cheap REST call; repeat for `phase:stalled`; `gh issue list --label` is the GraphQL-backed convenience form). This is the label-side mirror of source 3's board scan and works even when `release-state.json` is absent. An issue is *unresolved* when the user hasn't commented since the bounce-reason comment; fetch that comment's text for the item's description so the queue entry says what's blocked, not just that something is.
5. **Failing CI on your branches** — `gh run list --branch <branch> --status failure --limit 5` per checked-out branch with recent activity.
6. **Wish reports awaiting triage** — `ls wishes/` directories without corresponding filed issues. Cross-reference over REST search: `gh api -X GET search/issues -f q='repo:iamacoffeepot/aether <wish-slug>' --jq '.total_count'` (zero → unfiled).
7. **Pending memory writes** — entries in `~/.claude/projects/.../memory/PENDING.md` if the file exists (a thin convention: Claude appends "memory I think should be saved but you haven't decided on" entries; `/secretary` surfaces them as multi-choice).

Don't post a blocker for anything in this list — the scan finds them.

## Sources scanned (posted blockers)

Path: `~/.claude/projects/<project-slug>/secretary/blockers.jsonl` (`<project-slug>` is the Claude Code project directory that also holds your auto-memory `MEMORY.md` — the project's absolute path with each `/` replaced by `-`).

Format — one JSON object per line:

```json
{
  "id": "blkr-<uuid7>",
  "created_at": "2026-05-19T09:23:00Z",
  "type": "choice | document | memory | review",
  "title": "/implement #983 hit retry cap on flake test",
  "description": "...",
  "options": ["a) ...", "b) ...", "c) ..."],
  "source": "agent:a66563e1b6fc3586d / /implement",
  "resolved": false,
  "resolution": null
}
```

Posted blockers are HIGH priority unless the poster sets `priority: "low"` (rare; reserved for nice-to-have decisions).

`--clear-resolved` removes `resolved: true` entries from the file.

## Output format

Two-pass: print the ranked queue first (so the user can see everything at a glance), then call `AskUserQuestion` for actionable items.

**Pass 1 — print:**

```
SECRETARY — <ISO-date>

HIGH (<n>):

1. <title>
   Type: <choice|document|memory|review>
   <one-paragraph description>
   <if choice:>
     a) <option text>
     b) <option text>
     c) <option text>
   <if document:>
     Read: <command to view the artifact>
     Then: <command to act on it, or "use the selector below">

2. ...

MED (<n>):
   <same shape>

LOW (<n>):  (print-only, no selector)
   <same shape>

Total: <n>.
```

**Pass 2 — selector:** immediately after the print, call `AskUserQuestion` once for HIGH items, again (if needed) for MED items. LOW items are surfaced for awareness only — no selector pressure.

`AskUserQuestion` constraints (these are the load-bearing ones to plan around):

- Max 4 questions per call → batch items in groups of 4; if more remain, the user picks first 4, then Claude makes a second `AskUserQuestion` call.
- Each question has 2-4 options → if a posted blocker has 5+ options, take the top 3 and let the automatic "Other" cover the rest (the user can free-text the omitted choice).
- `header` field is max 12 chars → derive a short tag (`#973`, `flake-983`, `mem:audit-fp`, etc).
- `multiSelect: false` for these items — each prompt is one decision.

**Per-type option mapping:**

| Type | Default options | header |
|------|-----------------|--------|
| `choice` (posted blocker) | The blocker's own `options` array, truncated to 3 + auto-Other if >3 | first 12 chars of title |
| `document` — PR awaiting review | "Approve", "Bounce to Plan", "Skip for now" | `#NNN` |
| `document` — wish report awaiting triage | "File issues", "Skip for now", "Delete tree" | wish slug head |
| `document` — Bounced project item | "Re-scope (Plan)", "Re-define (Define)", "Skip for now" | `#NNN` |
| `memory` (pending memory write) | "Save", "Drop", "Edit then save" | `mem:slug` |
| `review` (changes-requested PR) | "Address comments", "Bounce to Plan", "Skip for now" | `#NNN` |

The first option is always the default-recommended action when one exists (e.g. for a PR with CI green and a clean diff, "Approve" goes first).

For document items the user typically wants to read first: present the selector with the "Open in browser" / "View first" action absent from the options (use the Read column above the selector) — the selector is for what to do *after* reading. If the user picks an action without reading, that's their call.

After the user picks, act on each one (run `/approve`, `/bounce`, save memory file, edit JSONL `resolved: true`, etc.) and report what was done.

## Priority rules

- **HIGH**: posted blockers (active agents are waiting); CI red on a branch with recent commits; Bounced / Stalled project items; PRs with explicit "changes requested" review.
- **MED**: open PRs >24h with no review; project items at `Phase=Plan + AgentReady=No`; failing CI on branches without recent commits.
- **LOW**: pending memory saves; old wish reports awaiting triage; stale unmerged branches with no PR.

If `--high` is passed, only HIGH items print.

## Posting a blocker (Claude/agent side)

When a long-running task needs a user decision and can't proceed without it, post a blocker BEFORE giving up on the task:

```
/secretary --post-blocker \
    --type choice \
    --title "/implement #983 hit retry cap on flake test" \
    --description "Test aether-substrate::actor::dispatch::tests::flake_under_load failed 3/3 with different errors each attempt — looks like a real flake, not a regression. Need a call on whether to bounce-to-Plan with a 'quarantine this test' note, mark as Stalled, or rerun with retry-cap=5." \
    --options "Bounce to Plan with quarantine note | Mark Stalled | Rerun with retry-cap=5 | Other (free text)" \
    --source "agent:a66563e1b6fc3586d /implement"
```

The blocker writes to the posted-blockers JSONL. The agent can then exit cleanly; the user sees the blocker on next `/secretary` invocation.

**Don't post a blocker for things observable elsewhere.** A PR awaiting review is already in the scan; posting a blocker about it is noise.

**Post sparingly.** A blocker is "I cannot proceed without you." If you can make a reasonable judgment call, do so and surface it as a decision in your reply, not a blocker.

## Resolution

**Selector path (primary):** the user clicks options in the `AskUserQuestion` UI. Claude reads the answers, then for each picked option:

1. Maps the option text back to its concrete action (`/approve <N>`, `/bounce <N> <phase> --reason "..."`, save a memory file, edit blocker JSONL `resolved: true`, etc.).
2. Runs the action and captures the result.
3. Confirms back what was done in a brief reply.

If the user picked **"Other"** for any item, Claude reads the free-text and acts on the user's intent — or, if it's ambiguous, asks one clarifying `AskUserQuestion` follow-up with a tighter option set before acting.

**Inline-text path (fallback):** if the user dismisses the selector and replies in chat instead — e.g. *"approve #973 and #976, bounce #984 to Plan with note 'needs the scenario harness from #868 first', skip the rest"* — parse the reply and act the same way. Both paths converge on the same action set.

**Clarification path:** if any item needs a longer conversation (the user asks *"why is #984 bounced?"* or *"show me the diff first"*), expand that item with a focused answer, then re-prompt the selector for the remaining items.

Posted blockers stay in the JSONL with `resolved: false` until explicitly resolved through either path. `--clear-resolved` garbage-collects the entries afterward.

## What `/secretary` does NOT do

- Resolve blockers automatically. The user decides.
- Modify project board state directly — that's `/approve`, `/bounce`, `/scope`'s job. `/secretary` calls them as instructed by the user reply.
- Post blockers for observable state. The scan finds those.
- Generate new work. It surfaces existing pending items.
- Page or notify out-of-band. The queue lives in the chat output.

## Failure modes

- **`gh` rate-limited** mid-scan: degrade gracefully — show what loaded, note the partial scan, suggest re-running later.
- **`.claude/release-state.json` missing**: skip the project-board scan, note in the report. Other sources still scan.
- **Posted-blockers file malformed** (a hand-edit broke the JSONL): skip the malformed line(s), include a warning at the bottom of the report.
- **No items pending**: report *"Inbox zero. Nothing pending — go ship something."*

## Storage

Posted blockers: `~/.claude/projects/<project-slug>/secretary/blockers.jsonl` (per-user, not in repo; same project directory as your auto-memory `MEMORY.md`).

The skill creates the directory if absent; the file is `chmod 644` (readable, not sensitive but personal).
