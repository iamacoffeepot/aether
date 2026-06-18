#!/usr/bin/env bash
# Fixture-based regression tests for the worktree-boundary guardrail hooks.
# Zero-dependency bash, matching the hooks' own style: feed each hook a crafted
# PreToolUse/PostToolUse stdin JSON inside a hermetic throwaway scaffold, and
# assert its exit code and (where it matters) a stdout substring. Run from
# anywhere; exits non-zero on any failed case.
#
# Coverage is the don't-dirty-main boundary hooks — check-worktree-boundary.sh
# (the PreToolUse edit ask-gate), check-worktree-clean.sh (the PostToolUse
# tripwire), and the session-worktree lock lifecycle (bind-session-worktree.sh
# locks on bind, release-session-worktree.sh unlocks on session end). The
# remaining wired hooks (check-pr-body, check-pre-push, check-host-fn-additions,
# check-no-divider-comments) are not yet covered here; the case table below is
# the place to add them.
#
# The stateful hooks key off whether the session has a worktree
# (.claude/worktrees/<session-id>) and inspect git state, so the scaffold is a
# real throwaway git repo with a per-session worktree and a committed tracked
# file, addressed via CLAUDE_PROJECT_DIR. It lives OUTSIDE the temp roots the
# edit gate sanctions as scratch (/tmp, /var/folders), so the gate actually
# evaluates instead of short-circuiting.
set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HOOKS=$(cd "$HERE/.." && pwd)

SCAFFOLD=$(mktemp -d "${HOME:-/tmp}/aether-hooktest.XXXXXX")
cleanup() { rm -rf "$SCAFFOLD"; }
trap cleanup EXIT

git -C "$SCAFFOLD" init -q
git -C "$SCAFFOLD" config user.email test@example.com
git -C "$SCAFFOLD" config user.name test
printf '/research/\n/.claude/worktrees/\n' > "$SCAFFOLD/.gitignore"
mkdir -p "$SCAFFOLD/src" "$SCAFFOLD/research"
printf 'fn main() {}\n' > "$SCAFFOLD/src/lib.rs"
git -C "$SCAFFOLD" add -A
git -C "$SCAFFOLD" commit -qm init
# A session is "bound" when it has a worktree; SESS has one, NONE does not.
git -C "$SCAFFOLD" worktree add -q "$SCAFFOLD/.claude/worktrees/SESS" >/dev/null 2>&1

pass=0; fail=0
# run <hook> <stdin-json>  ->  sets RC and OUT
run() { OUT=$(printf '%s' "$2" | CLAUDE_PROJECT_DIR="$SCAFFOLD" bash "$HOOKS/$1" 2>/dev/null); RC=$?; }
# expect <desc> <hook> <json> <rc> [stdout-substr]
expect() {
  local desc="$1" hook="$2" json="$3" want="$4" sub="${5:-}"
  run "$hook" "$json"
  if [ "$RC" != "$want" ]; then
    fail=$((fail+1)); printf 'FAIL  %-48s want rc=%s got rc=%s\n      out: %s\n' "$desc" "$want" "$RC" "$OUT"; return
  fi
  if [ -n "$sub" ] && ! printf '%s' "$OUT" | grep -q "$sub"; then
    fail=$((fail+1)); printf 'FAIL  %-48s stdout missing %s\n      out: %s\n' "$desc" "$sub" "$OUT"; return
  fi
  pass=$((pass+1)); printf 'PASS  %-48s [rc=%s]\n' "$desc" "$RC"
}
# expect_no <desc> <hook> <json> <substr>  — passes when substr is ABSENT and rc=0
expect_no() {
  local desc="$1" hook="$2" json="$3" sub="$4"
  run "$hook" "$json"
  if [ "$RC" = 0 ] && ! printf '%s' "$OUT" | grep -q "$sub"; then
    pass=$((pass+1)); printf 'PASS  %-48s [rc=0, no %s]\n' "$desc" "$sub"
  else
    fail=$((fail+1)); printf 'FAIL  %-48s want rc=0 and no %s; rc=%s out=%s\n' "$desc" "$sub" "$RC" "$OUT"
  fi
}

ASK='"permissionDecision":"ask"'

echo "## check-worktree-boundary.sh — PreToolUse edit ask-gate"
expect    "edit gate: write tracked main path -> ask"   check-worktree-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/src/lib.rs\"}}" 0 "$ASK"
expect_no "edit gate: gitignored path -> silent allow"  check-worktree-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/research/x.md\"}}" "$ASK"
expect_no "edit gate: own worktree -> silent allow"     check-worktree-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/.claude/worktrees/SESS/x\"}}" "$ASK"
expect_no "edit gate: /tmp scratch -> silent allow"     check-worktree-boundary.sh \
  '{"session_id":"SESS","tool_name":"Write","tool_input":{"file_path":"/tmp/x"}}' "$ASK"
expect_no "edit gate: Bash tool -> not gated here"      check-worktree-boundary.sh \
  '{"session_id":"SESS","tool_name":"Bash","tool_input":{"command":"git push"}}' "$ASK"
expect_no "edit gate: no worktree -> fail open"         check-worktree-boundary.sh \
  "{\"session_id\":\"NONE\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/src/lib.rs\"}}" "$ASK"

echo "## check-worktree-clean.sh — PostToolUse tripwire"
expect "clean main -> allow"          check-worktree-clean.sh '{"session_id":"SESS"}' 0
printf 'dirtied\n' >> "$SCAFFOLD/src/lib.rs"
expect "dirty main -> block (exit 2)" check-worktree-clean.sh '{"session_id":"SESS"}' 2
git -C "$SCAFFOLD" checkout -q -- src/lib.rs
expect "no worktree -> fail open"     check-worktree-clean.sh '{"session_id":"NONE"}' 0

echo "## bind-session-worktree.sh — locks the session worktree against removal"
expect "bind: exits 0 for a fresh session" bind-session-worktree.sh '{"session_id":"BINDLOCK"}' 0
# The worktree it created must now refuse a plain `git worktree remove` — the
# lock is what stops a /sweep or ad-hoc cleanup yanking a live session's tree.
if git -C "$SCAFFOLD" worktree remove "$SCAFFOLD/.claude/worktrees/BINDLOCK" >/dev/null 2>&1; then
  fail=$((fail+1)); printf 'FAIL  %-48s removal not refused (worktree unlocked)\n' "bind: locked worktree refuses removal"
else
  pass=$((pass+1)); printf 'PASS  %-48s [removal refused]\n' "bind: locked worktree refuses removal"
fi

echo "## release-session-worktree.sh — unlocks on session end"
git -C "$SCAFFOLD" worktree add -q "$SCAFFOLD/.claude/worktrees/RELSESS" >/dev/null 2>&1
git -C "$SCAFFOLD" worktree lock "$SCAFFOLD/.claude/worktrees/RELSESS" --reason "active claude session RELSESS" >/dev/null 2>&1
expect "release: exits 0" release-session-worktree.sh '{"session_id":"RELSESS"}' 0
# After release the lock is gone, so a plain remove now succeeds.
if git -C "$SCAFFOLD" worktree remove "$SCAFFOLD/.claude/worktrees/RELSESS" >/dev/null 2>&1; then
  pass=$((pass+1)); printf 'PASS  %-48s [unlocked, removable]\n' "release: worktree unlocked"
else
  fail=$((fail+1)); printf 'FAIL  %-48s still locked after release\n' "release: worktree unlocked"
fi
expect "release: no session id -> exit 0" release-session-worktree.sh '{}' 0

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
