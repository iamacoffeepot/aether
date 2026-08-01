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

# Rotate the log before (re)launching once it passes this size, keeping one
# previous generation. The in-process throttle bounds the common death spiral
# (issue 4042), but a long-lived tunnel still appends across many sessions and
# nothing else ever truncates this file — rotation is what keeps a fresh
# session from writing onto a multi-hundred-megabyte predecessor.
LOG_MAX_BYTES=$((32 * 1024 * 1024))

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

# Classify what is holding $TUNNEL_PORT. Echoes one of:
#
#   healthy    — our tunnel, both supervised children alive
#   degraded   — our tunnel, but a child is dead (issue 4039's zombie)
#   unknown    — answering, but not identifiably our tunnel
#
# The three-way split exists because the recovery below *kills a process*. A
# bound port is not health — a zombie tunnel keeps answering on :8890 while
# `hub` and `aether_mcp` fail to re-fork (stale binary paths from a deleted
# worktree is the case that bit a live session), which is `degraded` and is
# safe to replace. But an unreachable or unparseable `/admin/status` is NOT
# evidence of a broken tunnel: something else entirely may own the port, and
# killing it would be worse than any no-op. That is `unknown`, and it is
# reported rather than reaped.
#
# Liveness uses `jq` when present and otherwise counts `"alive":true` in the
# compact body — order-independent, so it never assumes which child serializes
# first.
tunnel_health() {
    command -v curl >/dev/null 2>&1 || { echo unknown; return; }
    local body
    body=$(curl -fsS --max-time 2 "$STATUS_URL" 2>/dev/null) || { echo unknown; return; }
    # Only our tunnel serves a status body carrying both child keys. Anything
    # else answering on this port is not ours to kill.
    if ! grep -q '"hub"' <<<"$body" || ! grep -q '"aether_mcp"' <<<"$body"; then
        echo unknown
        return
    fi
    if command -v jq >/dev/null 2>&1; then
        if jq -e '.children.hub.alive == true and .children.aether_mcp.alive == true' >/dev/null 2>&1 <<<"$body"; then
            echo healthy
        else
            echo degraded
        fi
        return
    fi
    if [[ $(grep -o '"alive":true' <<<"$body" | wc -l) -eq 2 ]]; then
        echo healthy
    else
        echo degraded
    fi
}

# Kill whatever holds $TUNNEL_PORT so the launch path below can replace it.
# Only ever called for a `degraded` verdict — a tunnel we positively
# identified, whose children are dead. Without this a dead-child tunnel keeps
# the port and every rerun no-ops.
reap_degraded_tunnel() {
    command -v lsof >/dev/null 2>&1 || return 1
    local pids
    pids=$(lsof -ti "tcp:${TUNNEL_PORT}" -sTCP:LISTEN 2>/dev/null) || true
    [[ -n "$pids" ]] || return 1
    echo "[ensure-tunnel] replacing degraded tunnel (pids: ${pids//$'\n'/ })"
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    # Give it a moment to release the port before we bind a fresh one.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        tunnel_is_up || return 0
        sleep 0.3
    done
    # shellcheck disable=SC2086
    kill -9 $pids 2>/dev/null || true
    return 0
}

# The :8890 check at the top is the double-launch guard: if a healthy tunnel is
# already bound we never launch a second one. An answering-but-unhealthy tunnel
# is torn down here so the launch path below replaces it, rather than being
# reported as success.
if tunnel_is_up; then
    case "$(tunnel_health)" in
        healthy)
            echo "[ensure-tunnel] tunnel already up on :${TUNNEL_PORT} — nothing to do."
            echo "run \`/mcp\` to (re)connect the harness tools if they're missing."
            exit 0
            ;;
        degraded)
            echo "[ensure-tunnel] :${TUNNEL_PORT} answers but a supervised child is dead — recovering."
            if ! reap_degraded_tunnel; then
                echo "[ensure-tunnel] could not reclaim :${TUNNEL_PORT} (no lsof, or nothing listening)." >&2
                echo "[ensure-tunnel] kill the process holding :${TUNNEL_PORT} and rerun." >&2
                exit 1
            fi
            ;;
        *)
            # Something owns the port but did not identify itself as our
            # tunnel. Never kill it — say what is true and let a human decide.
            echo "[ensure-tunnel] :${TUNNEL_PORT} is held by something that is not an aether-tunnel," >&2
            echo "[ensure-tunnel] or its /admin/status is unreadable. Not touching it." >&2
            echo "[ensure-tunnel] free the port (or set AETHER_TUNNEL_PORT) and rerun." >&2
            exit 1
            ;;
    esac
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
        -p aether-chassis-hub --bin aether-substrate-hub \
        -p aether-chassis-headless --bin aether-substrate-headless
) || true

# Bootstrap the hub's content-addressed binary store (ADR-0115) with the
# chassis bins just built, so a bare `spawn_substrate` (selector `default`)
# resolves to the headless binary even in a fresh or `restart-hub`'d hub —
# the spawn surface no longer takes a host path, so the host-path knowledge
# lives here in the build flow. The forked hub resolves this comma-separated
# list through `FleetConfig`'s `binary_bootstrap` field (its
# `AETHER_BINARY_BOOTSTRAP` env layer, ADR-0090) and ingests each (idempotent
# via content dedup). Exported so the detached tunnel — and the hub it forks —
# inherit it; only bins that actually built are listed.
BOOTSTRAP_BINS=()
for bin in aether-substrate-headless aether-substrate; do
    candidate="${PROJECT_DIR}/target/release/${bin}"
    [[ -x "$candidate" ]] && BOOTSTRAP_BINS+=("$candidate")
done
if (( ${#BOOTSTRAP_BINS[@]} > 0 )); then
    AETHER_BINARY_BOOTSTRAP="$(IFS=,; printf '%s' "${BOOTSTRAP_BINS[*]}")"
    export AETHER_BINARY_BOOTSTRAP
    echo "[ensure-tunnel] binary-store bootstrap: ${AETHER_BINARY_BOOTSTRAP}"
fi

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

# `wc -c` rather than `stat`: the BSD and GNU `stat` size flags differ and this
# script runs on both. Guarded by an existence test rather than a redirect
# fallback, so the first run of a session — where no log exists yet — stays
# silent instead of printing a shell "No such file or directory".
if [[ -f "$LOG_FILE" ]]; then
    log_bytes=$(( $(wc -c <"$LOG_FILE") ))
    if [[ "$log_bytes" -gt "$LOG_MAX_BYTES" ]]; then
        mv -f "$LOG_FILE" "${LOG_FILE}.1"
        echo "[ensure-tunnel] rotated ${log_bytes}-byte log to ${LOG_FILE}.1"
    fi
fi

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
        echo "run \`/mcp\` to (re)connect the harness tools if they're missing."
        exit 0
    fi
    sleep 1
done

echo "[ensure-tunnel] tunnel not up after ${STARTUP_TIMEOUT_SECS}s; continuing (see ${LOG_FILE})."
exit 0
