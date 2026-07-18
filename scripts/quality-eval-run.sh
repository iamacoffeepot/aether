#!/usr/bin/env bash
# Blind runner for the weekly offline quality eval (#3380). Reads the selector's
# NDJSON records on stdin (or a file arg) and, per sample, re-implements the
# closing issue BLIND, then harvests the candidate diff beside the landed "ground
# truth" diff. Emits one judge-input record per line:
#
#   {issue, candidate_diff, landed_diff, model, model_label, size_label}
#
# Blindness is STRUCTURAL, not incidental:
#   * The scratch repo is a single-revision clone — `git clone --revision=<parent>
#     --depth=1` (git >=2.49; the runner is 2.54) — so `parent_sha` is the ONLY
#     reachable commit and the landed squash commit is reachable from no ref.
#   * The clone's `origin` remote is removed immediately, sealing the repo so the
#     agent cannot fetch trunk even on a public repo.
#   * A pre-run assert — `git rev-list --all` must not contain `squash_sha` —
#     fails the sample loudly rather than reporting a possibly-peeked verdict.
#   * The isolated coding agent runs under an edit-only `--allowedTools` allowlist
#     with NO GITHUB token in its env (belt on top of the structural seal).
#
# The no-token / no-gh constraint scopes to the coding-agent invocation ONLY —
# this driver keeps git + network to clone and to harvest the landed diff.
#
# Env:
#   GITHUB_REPOSITORY            owner/repo (default iamacoffeepot/aether)
#   GITHUB_SERVER_URL            clone host origin (default https://github.com)
#   GITHUB_WORKSPACE             full-history checkout the landed diff is read from
#   QUALITY_EVAL_CLONE_TOKEN     optional token for cloning a private repo (sealed
#                                out with the remote; never reaches the agent)
#   QUALITY_EVAL_AGENT_TIMEOUT   per-sample agent wall-clock seconds (default 1800)
#   CLAUDE_CODE_OAUTH_TOKEN      forwarded to the coding-agent invocation

set -uo pipefail

repo="${GITHUB_REPOSITORY:-iamacoffeepot/aether}"
server="${GITHUB_SERVER_URL:-https://github.com}"
workspace="${GITHUB_WORKSPACE:-$PWD}"
agent_timeout="${QUALITY_EVAL_AGENT_TIMEOUT:-1800}"

# The label -> model mapping mirrors agent-work.yml's "Resolve the driving model"
# step (current fleet routing): opus for model:opus, else the headless sonnet
# string, so the eval runs each blind re-implementation under the model the
# pipeline would actually route that issue to.
resolve_model() {
  case "$1" in
    model:opus) echo "opus" ;;
    model:sonnet) echo "claude-sonnet-5" ;;
    *) echo "claude-sonnet-5" ;;
  esac
}

contaminated=0
processed=0

records_file="${1:-/dev/stdin}"

while IFS= read -r line; do
  [ -n "$line" ] || continue
  issue=$(jq -r '.issue' <<<"$line")
  parent_sha=$(jq -r '.parent_sha' <<<"$line")
  squash_sha=$(jq -r '.squash_sha' <<<"$line")
  model_label=$(jq -r '.model_label // ""' <<<"$line")
  size_label=$(jq -r '.size_label // ""' <<<"$line")
  issue_body=$(jq -r '.issue_body // ""' <<<"$line")
  model=$(resolve_model "$model_label")

  scratch=$(mktemp -d)
  echo "[#${issue}] blind run at parent ${parent_sha:0:12} (model ${model})" >&2

  # Single-revision clone: parent_sha is the only reachable commit. A token
  # (private-repo case) rides the clone URL and is sealed out with the remote.
  clone_url="${server}/${repo}.git"
  if [ -n "${QUALITY_EVAL_CLONE_TOKEN:-}" ]; then
    clone_url="https://x-access-token:${QUALITY_EVAL_CLONE_TOKEN}@${clone_url#https://}"
  fi
  if ! git clone --revision="$parent_sha" --depth=1 "$clone_url" "$scratch" 2>/dev/null; then
    echo "[#${issue}] SKIP: single-revision clone at ${parent_sha:0:12} failed" >&2
    rm -rf "$scratch"
    continue
  fi
  git -C "$scratch" remote remove origin 2>/dev/null || true

  # Contamination assert: the landed squash commit must be reachable from NO ref
  # in the sealed scratch. A hit means the clone leaked the truth — fail the
  # sample loudly rather than emit a possibly-peeked verdict (the exact risk the
  # 2026-07-14 bounce guards against).
  if ! revs=$(git -C "$scratch" rev-list --all); then
    echo "[#${issue}] SKIP: contamination check itself failed (rev-list error) — refusing to treat as clean" >&2
    rm -rf "$scratch"
    continue
  fi
  if grep -qx "$squash_sha" <<<"$revs"; then
    echo "[#${issue}] CONTAMINATED: squash ${squash_sha:0:12} reachable in the scratch clone — skipping" >&2
    contaminated=$((contaminated + 1))
    rm -rf "$scratch"
    continue
  fi

  # Strip interactive-session .claude hooks from the scratch copy (harmful in a
  # headless run — the SessionStart rebind would move the session out of scratch).
  if [ -f "$scratch/.claude/settings.json" ]; then
    tmp=$(mktemp)
    if ! jq 'del(.hooks)' "$scratch/.claude/settings.json" >"$tmp"; then
      echo "[#${issue}] SKIP: could not strip hooks from the scratch settings.json" >&2
      rm -f "$tmp"; rm -rf "$scratch"
      continue
    fi
    mv "$tmp" "$scratch/.claude/settings.json"
    git -C "$scratch" update-index --skip-worktree .claude/settings.json
  fi

  # Pre-trust the scratch project so settings.json applies without a blocking
  # trust prompt — needed because the belt below does NOT skip permissions.
  # untrust_scratch scrubs that entry again on every post-trust exit path, so
  # the shared home file does not accrete one dead entry per sample.
  untrust_scratch() {
    [ -f "$HOME/.claude.json" ] || return 0
    local t; t=$(mktemp)
    if jq --arg d "$scratch" 'del(.projects[$d])' "$HOME/.claude.json" >"$t"; then
      mv "$t" "$HOME/.claude.json"
    else
      rm -f "$t"
      echo "[#${issue}] warning: could not scrub trust entry for ${scratch}" >&2
    fi
  }
  if [ -f "$HOME/.claude.json" ]; then
    tmp=$(mktemp)
    jq --arg d "$scratch" '.projects[$d] = {"hasTrustDialogAccepted":true}' "$HOME/.claude.json" >"$tmp" && mv "$tmp" "$HOME/.claude.json"
  else
    printf '{"projects":{"%s":{"hasTrustDialogAccepted":true}}}\n' "$scratch" >"$HOME/.claude.json"
  fi

  # The isolated coding agent. Edit-only allowlist (Read/Edit/Write + navigation
  # + Bash for build/test); no WebFetch, no MCP (--strict-mcp-config), no gh. NO
  # GITHUB token in its env — even if it reached for `gh`/`git fetch` it has no
  # credentials, and the remote is already gone. Not permission-skipped, so any
  # tool outside the allowlist is denied. Time-boxed per sample.
  (
    cd "$scratch" || exit 1
    env -u GITHUB_TOKEN -u GH_TOKEN -u GH_ENTERPRISE_TOKEN -u GITHUB_ACTIONS \
      timeout "$agent_timeout" claude -p "$issue_body" \
      --model "$model" \
      --allowedTools "Read,Edit,Write,Grep,Glob,Bash" \
      --strict-mcp-config \
      --max-turns 60 \
      --output-format stream-json --verbose \
      >/tmp/quality-eval-agent-"${issue}".jsonl 2>&1
  ) || echo "[#${issue}] agent invocation exited non-zero (partial diff still harvested)" >&2

  # Harvest the candidate diff (staged so new files are included) and the landed
  # ground-truth diff (from the full-history workspace checkout).
  git -C "$scratch" add -A 2>/dev/null || true
  candidate_diff=$(git -C "$scratch" diff --cached 2>/dev/null || true)
  if ! landed_diff=$(git -C "$workspace" show "$squash_sha"); then
    echo "[#${issue}] SKIP: could not resolve landed diff for ${squash_sha}" >&2
    untrust_scratch
    rm -rf "$scratch"
    continue
  fi

  jq -nc \
    --argjson issue "$issue" \
    --arg candidate_diff "$candidate_diff" \
    --arg landed_diff "$landed_diff" \
    --arg model "$model" \
    --arg model_label "$model_label" \
    --arg size_label "$size_label" \
    '{issue: $issue, candidate_diff: $candidate_diff, landed_diff: $landed_diff, model: $model, model_label: $model_label, size_label: $size_label}'

  processed=$((processed + 1))
  untrust_scratch
  rm -rf "$scratch"
done <"$records_file"

echo "quality-eval-run: ${processed} sample(s) run, ${contaminated} contaminated-skipped" >&2
