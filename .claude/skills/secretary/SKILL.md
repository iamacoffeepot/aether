---
name: secretary
description: Surface everything that needs your attention as a ranked queue of documents-to-read and multi-choice prompts. Bidirectional — invoke as `/secretary` to see what's pending, or have Claude/agents post a blocker via `/secretary --post-blocker` when mid-task work needs a user decision. Scans observable state (open PRs, project board phases, failing CI, unanswered audit comments) plus a posted-blockers file. Output is two shapes: documents to read with a follow-up command, or multi-choice with explicit options.
---

# /secretary — pending-attention queue

`/secretary` is a scan-and-report skill. It tells the user what's pending their attention, grouped by priority, with each item shaped as either:

- **Document** — read this thing (PR / issue / wish report) and run a follow-up command.
- **Multi-choice** — pick one option from a short list to unblock work.

Bidirectional: the user invokes `/secretary` to see what's queued; Claude or any agent invokes `/secretary --post-blocker` when it's stuck mid-task and needs a user decision to continue. Posted blockers join the queue automatically.

Resolution is conversational. The user reads the queue, picks what to address, and replies in chat (*"approve #973 and #974, bounce #984 with reason X, save memories 1 and 3"*). No `--resolve` flag needed — the next-Claude-turn acts on the chat reply.

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

1. **Open PRs you authored** — `gh pr list --author @me --state open --json number,title,createdAt,reviewDecision,statusCheckRollup`. Items with no review and >24h old are MED; items with failing CI are HIGH.
2. **Open PRs from agents on your behalf** — `gh pr list --search "is:open author:app/*"` (or filter by branch prefix if agents use a convention).
3. **Active release project** (if `.claude/release-state.json` exists) — items at `Phase=Plan` with `AgentReady=No` (awaiting `/approve`), `Phase=Bounced` (need triage), `Phase=Stalled` (env/tooling).
4. **Issues with unanswered `[bounce]` / `[scope]` audit comments** since the last user reply on that issue.
5. **Failing CI on your branches** — `gh run list --branch <branch> --status failure --limit 5` per checked-out branch with recent activity.
6. **Wish reports awaiting triage** — `ls wishes/` directories without corresponding filed issues. Cross-reference against `gh issue list` body content (search for the wish slug).
7. **Pending memory writes** — entries in `~/.claude/projects/.../memory/PENDING.md` if the file exists (a thin convention: Claude appends "memory I think should be saved but you haven't decided on" entries; `/secretary` surfaces them as multi-choice).

Don't post a blocker for anything in this list — the scan finds them.

## Sources scanned (posted blockers)

Path: `~/.claude/projects/-Users-hadynfitzgerald-workspace-aether/secretary/blockers.jsonl`

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

```
SECRETARY — <ISO-date>

HIGH (<n>):

1. <title>
   Type: <choice|document>
   <one-paragraph description>
   <if choice:>
     a) <option text>
     b) <option text>
     c) <option text>
   <if document:>
     Read: <command to view the artifact>
     Then: <command to act on it, or "reply with decision">

2. ...

MED (<n>):
   <same shape>

LOW (<n>):
   <same shape>

Total: <n>. Reply with decisions inline, or:
  /secretary --high       refilter to just HIGH
  /secretary --since <D>  filter to recent
```

The output stays in chat (printed to stdout). No new file written by the read pass. The user replies inline; the next Claude turn parses the reply and acts.

## Priority rules

- **HIGH**: posted blockers (active agents are waiting); CI red on a branch with recent commits; Bounced / Stalled project items; PRs with explicit "changes requested" review.
- **MED**: open PRs >24h with no review; project items at `Phase=Plan + AgentReady=No`; unanswered audit comments; failing CI on branches without recent commits.
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

## Resolution (conversational, no special command)

The user reads the queue and replies in chat — e.g.:

> *Approve #973 and #976. Bounce #984 to Plan with note "needs the scenario harness from #868 first." Save memories 1 and 3. Skip the rest for now.*

The next Claude turn:

1. Parses the reply.
2. Runs `/approve 973`, `/approve 976`, `/bounce 984 Plan --reason "..."`, etc.
3. For each posted-blocker addressed, edits the JSONL entry to `resolved: true` with `resolution: <chosen-option-or-text>`.
4. Confirms back what was done.

If the user wants to handle one item interactively (asks a clarifying question, requests higher LOD), Claude expands that item before applying anything. The blockers stay queued until explicitly resolved.

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

Posted blockers: `~/.claude/projects/-Users-hadynfitzgerald-workspace-aether/secretary/blockers.jsonl` (per-user, not in repo).

The skill creates the directory if absent; the file is `chmod 644` (readable, not sensitive but personal).
