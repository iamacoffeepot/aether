#!/usr/bin/env bash
# Ensure the aether MCP tunnel is up. Claude runs this on demand when it
# needs the MCP harness — it is NOT auto-started on session start (a cold
# `cargo` build of the tunnel can take long enough to look like a frozen
# session, so the launch is left to the point of use).
#
# The tunnel (`aether-tunnel`, iamacoffeepot/aether#1212 PR 2) binds :8890 —
# the port `.mcp.json` targets — and forks + supervises `aether-mcp` (:8891)
# and the hub (:8901) behind it. This script is the idempotent bootstrap:
#
#   - If :8890 already answers, it is a no-op (the common case).
#   - Otherwise it launches the tunnel detached and waits, bounded, for the
#     port to come up.
#
# Bounded wait (so a cold build can't hang the caller indefinitely) and
# never-fatal (always exits 0 on the best-effort path): `set -e` is on
# inside the work, but the launch / probe path is guarded so a failed probe
# or launch can't propagate a non-zero exit.

set -euo pipefail

TUNNEL_PORT="${AETHER_TUNNEL_PORT:-8890}"
STATUS_URL="http://127.0.0.1:${TUNNEL_PORT}/admin/status"

# Where the detached tunnel's stdout/stderr go.
LOG_DIR="${TMPDIR:-/tmp}/aether-tunnel"
LOG_FILE="${LOG_DIR}/tunnel.log"

# How long to wait for a freshly-launched tunnel to bind :8890 before
# giving up (and still exiting 0).
STARTUP_TIMEOUT_SECS=15

# Resolve the project root so we can find a pre-built binary / run cargo.
# `CLAUDE_PROJECT_DIR` is set by the harness; fall back to the script's
# repo when run by hand.
PROJECT_DIR="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$PROJECT_DIR" ]]; then
    PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

# Load a developer-local .env if present (gitignored — never committed). Auto-export
# so the tunnel and everything it forks inherit it. No-op when the file is absent.
if [[ -f "$PROJECT_DIR/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    . "$PROJECT_DIR/.env"
    set +a
fi

# Probe :8890 — true if the tunnel is answering. Prefers the /admin/status
# HTTP probe (curl), falls back to a bare TCP connect (nc). Both swallow
# their own failure so the caller controls the exit.
tunnel_is_up() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsS --max-time 1 -o /dev/null "$STATUS_URL" 2>/dev/null && return 0
    fi
    if command -v nc >/dev/null 2>&1; then
        nc -z -w 1 127.0.0.1 "$TUNNEL_PORT" >/dev/null 2>&1 && return 0
    fi
    return 1
}

# The :8890 check at the top is the double-launch guard: if a tunnel is
# already bound we never launch a second one.
if tunnel_is_up; then
    echo "[ensure-tunnel] tunnel already up on :${TUNNEL_PORT} — nothing to do."
    exit 0
fi

# Pre-build every binary the tunnel will need to fork. `cargo run` below only
# builds `aether-tunnel`; in a fresh worktree where `target/release/` is empty
# the tunnel comes up and then fails to fork its children with
# `No such file or directory`. Naming each binary explicitly here keeps the
# fork chain build-complete on first invocation. Cargo no-ops when everything
# is current, so warm-target runs stay fast.
#
# Fork chain (extend this list if a new forked binary is added):
#   aether-tunnel        — the supervisor process itself (started below)
#   aether-mcp           — forked by the tunnel; speaks MCP to Claude
#   aether-substrate-hub — forked by the tunnel; the RPC server the fleet talks to
#   aether-substrate-headless — forked by the hub for `spawn_substrate`
echo "[ensure-tunnel] pre-building tunnel + forked binaries (no-op when warm)..."
(
    cd "$PROJECT_DIR" || exit 0
    cargo build --release \
        -p aether-mcp --bin aether-tunnel \
        -p aether-mcp --bin aether-mcp \
        -p aether-substrate-bundle --bin aether-substrate-hub \
        -p aether-substrate-bundle --bin aether-substrate-headless
) || true

# Pick a launch command: prefer a pre-built binary (fast, clean reap), else
# fall back to `cargo run` (rebuild-friendly).
RELEASE_BIN="${PROJECT_DIR}/target/release/aether-tunnel"
DEBUG_BIN="${PROJECT_DIR}/target/debug/aether-tunnel"
if [[ -x "$RELEASE_BIN" ]]; then
    LAUNCH=("$RELEASE_BIN")
elif [[ -x "$DEBUG_BIN" ]]; then
    LAUNCH=("$DEBUG_BIN")
else
    LAUNCH=(cargo run --release -p aether-mcp --bin aether-tunnel)
fi

mkdir -p "$LOG_DIR"
echo "[ensure-tunnel] :${TUNNEL_PORT} not answering — launching: ${LAUNCH[*]}"
echo "[ensure-tunnel] logs: ${LOG_FILE}"

# Launch detached: background, redirect output to the log, and disown so the
# tunnel outlives this hook process and the session. `|| true` keeps a spawn
# failure from tripping `set -e`.
(
    cd "$PROJECT_DIR" || exit 0
    nohup "${LAUNCH[@]}" >"$LOG_FILE" 2>&1 &
    disown
) || true

# Bounded wait for the tunnel to bind. Exit 0 the moment it answers; exit 0
# anyway if it never does — a SessionStart hook must not block the session.
deadline=$((SECONDS + STARTUP_TIMEOUT_SECS))
while (( SECONDS < deadline )); do
    if tunnel_is_up; then
        echo "[ensure-tunnel] tunnel is up on :${TUNNEL_PORT}."
        exit 0
    fi
    sleep 1
done

echo "[ensure-tunnel] tunnel not up after ${STARTUP_TIMEOUT_SECS}s; continuing (see ${LOG_FILE})."
exit 0
