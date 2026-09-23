#!/usr/bin/env bash
# Fixture-based regression tests for the guardrail hooks in .hooks/.
#
# Zero-dependency bash (3.2-compatible, like the hooks it runs): feed each hook
# a crafted hook-payload JSON on stdin inside a hermetic throwaway scaffold,
# and assert its exit code and, where it matters, a substring of its output.
# Run from anywhere; exits non-zero on any failed case.
#
# Every hook runs under `env -i` with only PATH, HOME, TMPDIR and
# CLAUDE_PROJECT_DIR (pointing at the scaffold), from the scaffold as cwd, so
# neither the caller's environment nor the checkout this script lives in can
# change a verdict.
#
# The scaffold is a real git repository with a committed tracked file, a
# gitignored scratch directory, a copy of .hooks/ (check-source-guardrails.sh
# runs its sibling from the repository it checks), and a bound session SESS
# laid out the way bind-session-worktree.sh lays one out: the real worktree at
# .agents/worktrees/SESS and a back-compat symlink at .claude/worktrees/SESS.
# It lives under HOME rather than a temp root, because check-worktree-boundary.sh
# allows /tmp and /var/folders as scratch and would never evaluate there.
set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HOOKS=$(cd "$HERE/.." && pwd)

SCAFFOLD=$(mktemp -d "${HOME:-/tmp}/aether-hooktest.XXXXXX")
cleanup() { rm -rf "$SCAFFOLD"; }
trap cleanup EXIT

sgit() { git -C "$SCAFFOLD" -c commit.gpgsign=false "$@"; }

sgit init -q
# Hook stderr is captured inside .git so it never shows in `git status`.
ERRF="$SCAFFOLD/.git/hooktest.stderr"
sgit config user.email test@example.com
sgit config user.name test
printf '/research/\n/.claude/worktrees/\n/.agents/worktrees/\n' > "$SCAFFOLD/.gitignore"
mkdir -p "$SCAFFOLD/src" "$SCAFFOLD/research" "$SCAFFOLD/.hooks"
printf 'fn main() {}\n' > "$SCAFFOLD/src/lib.rs"
cp "$HOOKS"/*.sh "$SCAFFOLD/.hooks/"
sgit add -A
sgit commit -qm init

bind_like() {
    local key="$1"
    sgit worktree add -q --detach "$SCAFFOLD/.agents/worktrees/$key" HEAD >/dev/null 2>&1
    mkdir -p "$SCAFFOLD/.claude/worktrees"
    ln -sfn "$SCAFFOLD/.agents/worktrees/$key" "$SCAFFOLD/.claude/worktrees/$key"
}
# A session is bound when it has a worktree; SESS has one, NONE does not.
bind_like SESS

# Restore the scaffold's main checkout to its committed state.
reset_main() {
    sgit checkout -q -- .
    sgit clean -qfd -- src
}

pass=0
fail=0
# Per-case knobs, reset after every case: the cwd the hook runs from, and extra
# VAR=value pairs for its environment.
CWD="$SCAFFOLD"
ENVX=()

# run <hook> <stdin-json>  ->  sets RC, OUT (stdout) and ERR (stderr)
run() {
    OUT=$(cd "$CWD" && printf '%s' "$2" \
        | env -i PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" CLAUDE_PROJECT_DIR="$SCAFFOLD" \
            ${ENVX[@]+"${ENVX[@]}"} bash "$HOOKS/$1" 2>"$ERRF")
    RC=$?
    ERR=$(cat "$ERRF")
    CWD="$SCAFFOLD"
    ENVX=()
}
ok() { pass=$((pass + 1)); printf 'PASS  %-58s %s\n' "$1" "$2"; }
bad() { fail=$((fail + 1)); printf 'FAIL  %-58s %s\n      stdout: %s\n      stderr: %s\n' "$1" "$2" "$OUT" "$ERR"; }
# expect <desc> <hook> <json> <rc> [output-substr]  (substr searched in stdout+stderr)
expect() {
    local desc="$1" hook="$2" json="$3" want="$4" sub="${5:-}"
    run "$hook" "$json"
    if [ "$RC" != "$want" ]; then
        bad "$desc" "want rc=$want got rc=$RC"
        return
    fi
    if [ -n "$sub" ] && ! printf '%s\n%s' "$OUT" "$ERR" | grep -qF -- "$sub"; then
        bad "$desc" "output missing: $sub"
        return
    fi
    ok "$desc" "[rc=$RC]"
}
# expect_no <desc> <hook> <json> <substr>  — rc=0 and substr absent from stdout
expect_no() {
    local desc="$1" hook="$2" json="$3" sub="$4"
    run "$hook" "$json"
    if [ "$RC" = 0 ] && ! printf '%s' "$OUT" | grep -qF -- "$sub"; then
        ok "$desc" "[rc=0, no $sub]"
    else
        bad "$desc" "want rc=0 and no $sub; rc=$RC"
    fi
}
# assert <desc> <command...>  — a direct check on scaffold state
assert() {
    local desc="$1"
    shift
    OUT=""
    ERR=""
    if "$@" >/dev/null 2>&1; then ok "$desc" "[ok]"; else bad "$desc" "check failed: $*"; fi
}
refute() {
    local desc="$1"
    shift
    OUT=""
    ERR=""
    if "$@" >/dev/null 2>&1; then bad "$desc" "unexpectedly succeeded: $*"; else ok "$desc" "[refused]"; fi
}
# json <tool_name> <tool_input-object>
tool() { jq -nc --arg t "$1" --argjson i "$2" '{session_id:"SESS",tool_name:$t,tool_input:$i}'; }
bashcmd() { jq -nc --arg c "$1" '{session_id:"SESS",tool_name:"Bash",tool_input:{command:$c}}'; }

ASK='"permissionDecision":"ask"'

echo "## check-worktree-boundary.sh — PreToolUse edit ask-gate"
expect "edit gate: write tracked main path -> ask" check-worktree-boundary.sh \
    "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/src/lib.rs\"}}" 0 "$ASK"
expect "edit gate: relative path resolves against main -> ask" check-worktree-boundary.sh \
    '{"session_id":"SESS","tool_name":"Edit","tool_input":{"file_path":"src/lib.rs"}}' 0 "$ASK"
expect_no "edit gate: gitignored path -> silent allow" check-worktree-boundary.sh \
    "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/research/x.md\"}}" "$ASK"
expect_no "edit gate: own worktree via legacy symlink -> allow" check-worktree-boundary.sh \
    "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/.claude/worktrees/SESS/x\"}}" "$ASK"
expect_no "edit gate: own worktree via real path -> allow" check-worktree-boundary.sh \
    "{\"session_id\":\"SESS\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/.agents/worktrees/SESS/x\"}}" "$ASK"
expect_no "edit gate: /tmp scratch -> silent allow" check-worktree-boundary.sh \
    '{"session_id":"SESS","tool_name":"Write","tool_input":{"file_path":"/tmp/x"}}' "$ASK"
expect_no "edit gate: Bash tool -> not gated here" check-worktree-boundary.sh \
    '{"session_id":"SESS","tool_name":"Bash","tool_input":{"command":"git push"}}' "$ASK"
expect_no "edit gate: no worktree -> fail open" check-worktree-boundary.sh \
    "{\"session_id\":\"NONE\",\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$SCAFFOLD/src/lib.rs\"}}" "$ASK"

echo "## check-worktree-clean.sh — PostToolUse don't-dirty-main tripwire"
expect "clean main -> allow" check-worktree-clean.sh '{"session_id":"SESS"}' 0
printf 'dirtied\n' >> "$SCAFFOLD/src/lib.rs"
expect "dirty main, bound session -> block (exit 2)" check-worktree-clean.sh '{"session_id":"SESS"}' 2 "main worktree is now dirty"
expect "dirty main, unbound session -> block via main-root check" check-worktree-clean.sh '{"session_id":"NONE"}' 2 "primary checkout is dirty"
CWD="$SCAFFOLD/.agents/worktrees/SESS"
expect "dirty main, no session id, run from a worktree -> block" check-worktree-clean.sh '{}' 2 "primary checkout is dirty"
ENVX=(AETHER_CODEX_ALLOW_DIRTY_MAIN=1)
expect "dirty main, AETHER_CODEX_ALLOW_DIRTY_MAIN=1 -> allow" check-worktree-clean.sh '{"session_id":"SESS"}' 0
reset_main
expect "clean main, unbound session -> allow" check-worktree-clean.sh '{"session_id":"NONE"}' 0

echo "## check-main-worktree-clean.sh — Codex main-root tripwire"
expect "clean main -> allow" check-main-worktree-clean.sh '{}' 0
printf 'dirtied\n' >> "$SCAFFOLD/src/lib.rs"
CWD="$SCAFFOLD/.agents/worktrees/SESS"
expect "dirty main, run from a worktree -> block (exit 2)" check-main-worktree-clean.sh '{}' 2 "primary checkout is dirty"
ENVX=(AETHER_CODEX_ALLOW_DIRTY_MAIN=1)
expect "dirty main, AETHER_CODEX_ALLOW_DIRTY_MAIN=1 -> allow" check-main-worktree-clean.sh '{}' 0
reset_main

echo "## check-no-divider-comments.sh — PreToolUse banner-comment gate"
expect "Edit adds // ---- to .rs -> block" check-no-divider-comments.sh \
    "$(tool Edit '{"file_path":"x.rs","old_string":"a","new_string":"// ---- banner ----\na"}')" 2 "no-divider-comments"
expect "Write adds # ==== to .py -> block" check-no-divider-comments.sh \
    "$(tool Write '{"file_path":"/nonexistent/x.py","content":"# ====\nx = 1\n"}')" 2 "no-divider-comments"
expect "Edit keeps an existing banner -> allow" check-no-divider-comments.sh \
    "$(tool Edit '{"file_path":"x.rs","old_string":"// ----\na","new_string":"// ----\nb"}')" 0
expect "banner with DIVIDER_OK override -> allow" check-no-divider-comments.sh \
    "$(tool Edit '{"file_path":"x.rs","old_string":"a","new_string":"// DIVIDER_OK: table\n// ----\na"}')" 0
expect "banner in a .md file -> not gated" check-no-divider-comments.sh \
    "$(tool Write '{"file_path":"x.md","content":"# ----\n"}')" 0
expect "MultiEdit adds a banner in one edit -> block" check-no-divider-comments.sh \
    "$(tool MultiEdit '{"file_path":"x.ts","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"// ***\nc"}]}')" 2 "1 new section-divider"
printf 'fn a() {}\n// ----\n' > "$SCAFFOLD/research/existing.rs"
expect "Write over a file that already has the banner -> allow" check-no-divider-comments.sh \
    "$(tool Write "$(jq -nc --arg p "$SCAFFOLD/research/existing.rs" '{file_path:$p,content:"// ----\nfn b() {}\n"}')")" 0

echo "## check-host-fn-additions.sh — PreToolUse host-fn gate"
expect "Edit adds linker.func_wrap to host_fns.rs -> block" check-host-fn-additions.sh \
    "$(tool Edit '{"file_path":"crates/x/src/host_fns.rs","old_string":"a","new_string":"linker.func_wrap(\"m\", \"f\", f)?;\na"}')" 2 "host_fns.rs"
expect "Edit with HOST_FN_OK override -> allow" check-host-fn-additions.sh \
    "$(tool Edit '{"file_path":"crates/x/src/host_fns.rs","old_string":"a","new_string":"// HOST_FN_OK: ffi\nlinker.func_wrap(\"m\", \"f\", f)?;\na"}')" 0
expect "Edit moving an existing func_wrap -> allow" check-host-fn-additions.sh \
    "$(tool Edit '{"file_path":"crates/x/src/host_fns.rs","old_string":"linker.func_wrap(a)","new_string":"linker.func_wrap(b)"}')" 0
expect "Write a new host_fns.rs with func_wrap -> block" check-host-fn-additions.sh \
    "$(tool Write '{"file_path":"/nonexistent/host_fns.rs","content":"linker.func_wrap(a);\n"}')" 2
expect "func_wrap in another file -> not gated" check-host-fn-additions.sh \
    "$(tool Edit '{"file_path":"crates/x/src/lib.rs","old_string":"a","new_string":"linker.func_wrap(a);"}')" 0

echo "## check-source-guardrails.sh — Codex PostToolUse diff guardrails"
expect "clean tree -> allow" check-source-guardrails.sh '{}' 0
printf '// ---- banner ----\n' >> "$SCAFFOLD/src/lib.rs"
expect "unstaged diff adds a banner -> block" check-source-guardrails.sh '{}' 2 "this diff adds section-divider"
sgit add src/lib.rs
expect "staged diff adds a banner -> block" check-source-guardrails.sh '{}' 2 "this diff adds section-divider"
sgit reset -q
reset_main
printf '# ====\n' > "$SCAFFOLD/src/new.sh"
expect "untracked source file with a banner -> block" check-source-guardrails.sh '{}' 2 "untracked file src/new.sh"
# Unlike the PreToolUse gate, this check exempts only a banner line that carries
# the marker itself; a marker on another line does not cover it.
printf '# DIVIDER_OK: table\n# ====\n' > "$SCAFFOLD/src/new.sh"
expect "untracked banner, DIVIDER_OK on another line -> block" check-source-guardrails.sh '{}' 2 "untracked file src/new.sh"
printf '# ==== # DIVIDER_OK: table\n' > "$SCAFFOLD/src/new.sh"
expect "untracked banner carrying DIVIDER_OK -> allow" check-source-guardrails.sh '{}' 0
reset_main
printf 'linker.func_wrap(a);\n' > "$SCAFFOLD/src/host_fns.rs"
expect "untracked host_fns.rs with func_wrap -> block" check-source-guardrails.sh '{}' 2 "adds linker.func_wrap"
printf '// HOST_FN_OK: ffi\nlinker.func_wrap(a);\n' > "$SCAFFOLD/src/host_fns.rs"
expect "untracked host_fns.rs with HOST_FN_OK -> allow" check-source-guardrails.sh '{}' 0
reset_main
printf '// ---- banner ----\n' > "$SCAFFOLD/research/scratch.rs"
expect "gitignored file with a banner -> allow" check-source-guardrails.sh '{}' 0
rm -f "$SCAFFOLD/research/scratch.rs"

echo "## check-pr-body.sh — PreToolUse PR/issue text gate"
expect "non-gh command -> allow" check-pr-body.sh "$(bashcmd 'git status')" 0
expect "clean PR -> allow" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "chore(repo): tidy" --body "plain body with `code`"')" 0
expect "PR body with a dollar math span -> block (D)" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "chore(repo): tidy" --body "costs $x and $y"')" 2 "Pattern D"
expect "PR body with a shell expansion -> allow" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "chore(repo): tidy" --body "run $(date) then $ 5"')" 0
expect "heredoc body with an escaped backtick -> block (A)" check-pr-body.sh \
    "$(bashcmd "$(printf 'gh pr create --title "chore(repo): tidy" --body-file - <<'"'"'EOF'"'"'\nuse \\`code\\`\nEOF')")" 2 "Pattern A"
printf 'value $a$ here\n' > "$SCAFFOLD/research/body.md"
expect "--body-file with a math span -> block (D)" check-pr-body.sh \
    "$(bashcmd "gh pr create --title 'chore(repo): tidy' --body-file $SCAFFOLD/research/body.md")" 2 "Pattern D"
expect "uppercase PR title subject -> block (C)" check-pr-body.sh \
    "$(bashcmd 'gh pr edit 1 --title "chore(repo): Tidy"')" 2 "Pattern C"
expect "override naming the pattern -> allow" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "chore(repo): tidy" --body "costs $x and $y <!-- pr-body-ok: d - literal -->"')" 0
expect "override with no pattern letter -> block" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "chore(repo): tidy" --body "x <!-- pr-body-ok: -->"')" 2 "needs at least one pattern letter"
expect "malformed issue title -> block (E)" check-pr-body.sh \
    "$(bashcmd 'gh issue create --title "tidy things" --body "x"')" 2 "Pattern E"
expect "issue title with a meta-scope -> allow" check-pr-body.sh \
    "$(bashcmd 'gh issue create --title "chore(repo): tidy things" --body "x"')" 0
expect "PR title outside the issue grammar -> allow (E is issue-only)" check-pr-body.sh \
    "$(bashcmd 'gh pr create --title "test(repo): tidy" --body "x"')" 0

echo "## bind-session-worktree.sh — creates, locks, and aliases the session worktree"
expect "bind: fresh session -> exit 0 with context" bind-session-worktree.sh \
    '{"session_id":"BINDLOCK"}' 0 ".agents/worktrees/BINDLOCK"
assert "bind: real worktree at .agents/worktrees/<id>" test -d "$SCAFFOLD/.agents/worktrees/BINDLOCK"
assert "bind: legacy symlink at .claude/worktrees/<id>" test -L "$SCAFFOLD/.claude/worktrees/BINDLOCK"
# The lock is what stops a /sweep or ad-hoc cleanup yanking a live session's tree.
refute "bind: locked worktree refuses removal" sgit worktree remove "$SCAFFOLD/.agents/worktrees/BINDLOCK"
expect "bind: rerun on an existing worktree -> exit 0" bind-session-worktree.sh \
    '{"session_id":"BINDLOCK"}' 0 ".agents/worktrees/BINDLOCK"
expect "bind: session id is sanitized into the key" bind-session-worktree.sh \
    '{"session_id":"odd/id with space"}' 0 ".agents/worktrees/odd-id-with-space"

echo "## release-session-worktree.sh — unlocks the real worktree on session end"
expect "release: exits 0" release-session-worktree.sh '{"session_id":"BINDLOCK"}' 0
# After release the lock is gone, so a plain remove now succeeds.
assert "release: bound worktree unlocked and removable" sgit worktree remove "$SCAFFOLD/.agents/worktrees/BINDLOCK"
rm -f "$SCAFFOLD/.claude/worktrees/odd-id-with-space"
expect "release: sanitized key, legacy symlink absent -> exit 0" release-session-worktree.sh \
    '{"session_id":"odd/id with space"}' 0
assert "release: unlocked at the real path without the symlink" \
    sgit worktree remove "$SCAFFOLD/.agents/worktrees/odd-id-with-space"
sgit worktree add -q --detach "$SCAFFOLD/.agents/worktrees/NOENV" HEAD
sgit worktree lock "$SCAFFOLD/.agents/worktrees/NOENV" --reason "active session NOENV"
# Without CLAUDE_PROJECT_DIR the hook resolves the root from its own location,
# so run the scaffold's copy of it.
OUT=$(cd "$SCAFFOLD" && printf '{"session_id":"NOENV"}' \
    | env -i PATH="$PATH" HOME="$HOME" bash "$SCAFFOLD/.hooks/release-session-worktree.sh" 2>"$ERRF")
RC=$?
ERR=$(cat "$ERRF")
if [ "$RC" = 0 ]; then ok "release: no CLAUDE_PROJECT_DIR -> exit 0" "[rc=0]"; else bad "release: no CLAUDE_PROJECT_DIR -> exit 0" "rc=$RC"; fi
assert "release: root resolved one level above .hooks/" sgit worktree remove "$SCAFFOLD/.agents/worktrees/NOENV"
expect "release: no session id -> exit 0" release-session-worktree.sh '{}' 0
expect "release: unknown session -> exit 0" release-session-worktree.sh '{"session_id":"GONE"}' 0

echo "## bind-session-worktree-codex.sh — Codex session worktree"
expect "codex bind: thread id -> worktree and context" bind-session-worktree-codex.sh \
    '{"thread_id":"t1"}' 0 ".agents/worktrees/codex-t1"
assert "codex bind: worktree created" test -d "$SCAFFOLD/.agents/worktrees/codex-t1"
expect "codex bind: no id -> silent" bind-session-worktree-codex.sh '{}' 0
if [ -z "$OUT" ]; then ok "codex bind: no id -> no output" "[empty]"; else bad "codex bind: no id -> no output" "unexpected stdout"; fi

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
