#!/usr/bin/env bash
# agent-tick-lib.sh — shared shell for .github/workflows/agent-tick.yml.
#
# Sourced (never executed) by the workflow's dispatch, stall, and nudge steps,
# each of which runs `actions/checkout@v4` ahead of its `run:` body so this file
# is on disk at the repo root — a plain `source scripts/agent-tick-lib.sh` needs
# no `git show`/`/tmp` staging. It defines functions and constants only — no
# top-level side effects beyond the two constant assignments — so it is safe to
# source under `set -euo pipefail`. All reads key on the job-level $REPO / $OWNER
# env, which is in scope in every step.
# shellcheck shell=bash

# Scope-eligibility. Backlog means "no phase:* label", which is also true of
# every machine-filed alert that lands in the repo — and the tick used to scope
# those. The context-budget canary and the nightly fuzzer both file issues with
# NO labels at all, so the fleet had a standing feed of alerts it would walk
# through Define/Design/Plan, each costing a full Claude run to discover it
# should not have (#3179 was scoped for seven minutes before being cancelled by
# hand).
#
# Filtering by AUTHOR would be wrong: the fleet's own App files real work — a
# scoping agent split the fat #3145 into #3162 and #3163, and those must be
# scoped. The signal that separates them is `type:*`. Every real work item
# carries one (/sketch stamps it from the conventional-commit title, and split
# children inherit it); an alert filed outside the pipeline carries none.
#
# AGENT_SCOPE_TYPES is the owner's dial for WHICH KINDS of work the fleet takes
# on, alongside the other governors. An issue whose type is not in the list is
# left alone, and so is anything tagged `alert`. Fail-safe: an issue the tick
# cannot classify is NOT scoped — silence costs a human one label, the opposite
# costs a Claude run and a design nobody asked for.
scope_types="${SCOPE_TYPES:-feat,fix,chore,docs,perf,refactor,flake}"
scopeable() {
  case ",${scope_types}," in
    *",$1,"*) return 0 ;;
    *) return 1 ;;
  esac
}

# Board scan → advanceable snapshot. Open non-PR issues with labels;
# `has("pull_request")|not` drops PRs (they share the issues endpoint).
# `type` and `alert` back the scope-eligibility gate below.
# The await read keys on the agent:awaiting-answer label alone —
# the canonical parked-state store every park writes — never a
# free-text scan: an issue that merely QUOTES the marker in prose
# (backticked, mid-sentence, unclosed) must not self-park (#3330
# sat unlabeled and undispatchable because its own body documented
# the marker).
# Emits the canonical 7-column TSV row (num phase await dt itype alert preapp)
# to stdout; callers redirect. The stall step reads all 7 fields and simply
# omits `preapp` from its gate — the one scan feeds every consumer.
board_scan() {
  gh api "repos/${REPO}/issues?state=open&per_page=100" --paginate \
    --jq '.[] | select(has("pull_request")|not)
      | [ .number,
          ([.labels[].name | select(startswith("phase:"))] | .[0] // "none"),
          (if ([.labels[].name] | index("agent:awaiting-answer")) then "await" else "-" end),
          (if ([.labels[].name] | index("agent:dont-touch")) then "dt" else "-" end),
          ([.labels[].name | select(startswith("type:")) | ltrimstr("type:")] | .[0] // "-"),
          (if ([.labels[].name] | index("alert")) then "alert" else "-" end),
          (if ([.labels[].name] | index("approval:pre-approved")) then "pre" else "-" end) ]
      | @tsv'
}

# Park-anchored owner-reply read (#3330). The old read took only the
# LATEST comment author, and agent-chat acks every owner comment on
# a parked issue — the ack landed after the reply, so the newest
# author was always the bot and owner_answered could never become
# true (observed on #3316: the park was unclearable by commenting).
# Anchor on the park moment instead and ask whether ANY comment
# after it is owner-authored — no later automated comment can bury
# the reply. The anchor is the newest agent:awaiting-answer label
# event, structured GitHub data every park emits.
# RFC3339-Z timestamps (what the API returns) compare
# chronologically as plain strings. Comments come oldest-first;
# head/tail -n1 under `|| true` is the file's SIGPIPE-safe idiom.
park_anchor() {
  local ts
  ts="$(gh api "repos/${REPO}/issues/${1}/timeline?per_page=100" --paginate \
    --jq '[.[] | select(.event=="labeled" and .label.name=="agent:awaiting-answer") | .created_at] | last // empty' \
    | tail -n1 || true)"
  printf '%s' "$ts"
}

# The owner's first comment strictly after $2 (a park anchor) — empty when no
# such comment exists. Shared by the dispatch step's owner_answer_ts and the
# nudge step's already-answered check, which both key on "owner replied after
# the park moment".
owner_reply_after() {
  gh api "repos/${REPO}/issues/${1}/comments?per_page=100" --paginate \
    --jq ".[] | select(.user.login == \"${OWNER}\" and .created_at > \"${2}\") | .created_at" \
    | head -n1 || true
}

# The owner's first comment strictly after the park anchor — empty
# when the question is unanswered or no park anchor exists.
owner_answer_ts() {
  local park
  park="$(park_anchor "$1")"
  [ -n "$park" ] || return 0
  owner_reply_after "$1" "$park"
}

owner_answered() {
  [ -n "$(owner_answer_ts "$1")" ]
}

# A rescued answer older than one tick interval (the ~20m schedule
# cadence) means the primary agent-chat path had its chance and did NOT
# clear the park: the tick backstop is rescuing a silently-stranded
# answer, which warrants a visible alert (#3265). The grace is one tick
# interval so a freshly-answered surface the tick legitimately picks up
# is never alerted. Ages the owner's park-anchored reply itself, not
# the newest comment — a later bot ack must not reset the clock.
STRAND_GRACE_SECS=1200
answer_stale() {
  local ts age
  ts="$(owner_answer_ts "$1")"
  [ -n "$ts" ] || return 1
  age=$(( $(date -u +%s) - $(date -u -d "$ts" +%s) ))
  [ "$age" -ge "$STRAND_GRACE_SECS" ]
}
