#!/usr/bin/env bash
# Fixture-based regression tests for the role-model guardrail hooks
# (ADR-0110 / ADR-0111). Zero-dependency bash, matching the hooks' own style:
# feed each hook a crafted PreToolUse/PostToolUse stdin JSON inside a hermetic
# throwaway scaffold, and assert its exit code and (where it matters) a stdout
# substring. Run from anywhere; exits non-zero on any failed case.
#
# Coverage is the two role-model boundary hooks — check-role-boundary.sh (the
# PreToolUse ask-gates) and check-worktree-clean.sh (the PostToolUse tripwire).
# The other wired hooks (check-pr-body, check-pre-push, check-host-fn-additions,
# check-no-divider-comments, bind-session-role) are not yet covered here; the
# case table below is the place to add them.
#
# The stateful hooks read a session role marker and inspect git state, so the
# scaffold is a real throwaway git repo with a role marker and a committed
# tracked file, addressed via CLAUDE_PROJECT_DIR. It lives OUTSIDE the temp
# roots the edit gate sanctions as scratch (/tmp, /var/folders), so the gate
# actually evaluates instead of short-circuiting.
set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HOOKS=$(cd "$HERE/.." && pwd)

SCAFFOLD=$(mktemp -d "${HOME:-/tmp}/aether-hooktest.XXXXXX")
cleanup() { rm -rf "$SCAFFOLD"; }
trap cleanup EXIT

git -C "$SCAFFOLD" init -q
git -C "$SCAFFOLD" config user.email test@example.com
git -C "$SCAFFOLD" config user.name test
printf '/research/\n/.claude/roles/\n/.claude/worktrees/\n' > "$SCAFFOLD/.gitignore"
mkdir -p "$SCAFFOLD/.claude/roles" "$SCAFFOLD/src" "$SCAFFOLD/research" "$SCAFFOLD/.claude/worktrees/SESS"
printf 'dreamer\n' > "$SCAFFOLD/.claude/roles/SESS"
printf 'orchestrator\n' > "$SCAFFOLD/.claude/roles/ORCH"
printf 'everything\n' > "$SCAFFOLD/.claude/roles/EVERY"
printf 'fn main() {}\n' > "$SCAFFOLD/src/lib.rs"
git -C "$SCAFFOLD" add -A
git -C "$SCAFFOLD" commit -qm init

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

echo "## check-role-boundary.sh — PreToolUse ask-gates"
expect    "role gate: dreamer git push -> ask"        check-role-boundary.sh \
  '{"session_id":"SESS","tool_name":"Bash","tool_input":{"command":"git push"}}' 0 "$ASK"
expect    "role gate: dreamer gh pr merge -> ask"      check-role-boundary.sh \
  '{"session_id":"SESS","tool_name":"Bash","tool_input":{"command":"gh pr merge 1"}}' 0 "$ASK"
expect    "role gate: orchestrator gh issue create -> ask" check-role-boundary.sh \
  '{"session_id":"ORCH","tool_name":"Bash","tool_input":{"command":"gh issue create -t x"}}' 0 "$ASK"
expect    "edit gate: dreamer write tracked path -> ask" check-role-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/src/lib.rs\"}}" 0 "$ASK"
expect_no "edit gate: gitignored path -> silent allow"  check-role-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/research/x.md\"}}" "$ASK"
expect_no "edit gate: own worktree -> silent allow"     check-role-boundary.sh \
  "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/.claude/worktrees/SESS/x\"}}" "$ASK"
expect_no "edit gate: /tmp scratch -> silent allow"     check-role-boundary.sh \
  '{"session_id":"SESS","tool_name":"Write","tool_input":{"file_path":"/tmp/x"}}' "$ASK"
expect_no "everything role -> no boundary"              check-role-boundary.sh \
  '{"session_id":"EVERY","tool_name":"Bash","tool_input":{"command":"git push"}}' "$ASK"
expect_no "unbound session -> fail open"                check-role-boundary.sh \
  '{"session_id":"NONE","tool_name":"Bash","tool_input":{"command":"git push"}}' "$ASK"
expect_no "harmless read command -> allow"              check-role-boundary.sh \
  '{"session_id":"SESS","tool_name":"Bash","tool_input":{"command":"git status"}}' "$ASK"

echo "## check-worktree-clean.sh — PostToolUse tripwire"
expect "clean main -> allow"          check-worktree-clean.sh '{"session_id":"SESS"}' 0
printf 'dirtied\n' >> "$SCAFFOLD/src/lib.rs"
expect "dirty main -> block (exit 2)" check-worktree-clean.sh '{"session_id":"SESS"}' 2
git -C "$SCAFFOLD" checkout -q -- src/lib.rs
expect "unbound session -> fail open" check-worktree-clean.sh '{"session_id":"NONE"}' 0

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
