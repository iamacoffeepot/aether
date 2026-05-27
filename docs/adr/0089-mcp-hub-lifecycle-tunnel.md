# ADR-0089: Claude-managed MCP/hub lifecycle tunnel

- **Status:** Proposed
- **Date:** 2026-05-26

## Context

Claude drives a live engine through MCP (the harness described in `CLAUDE.md`). The dev workflow is two hand-started long-lived processes:

```
cargo run -p aether-substrate-bundle --bin aether-substrate-hub   # RPC :8901
cargo run -p aether-mcp                                           # MCP HTTP :8890
```

Two forces make this awkward to operate, and specifically make it something Claude *cannot* manage itself:

1. **A hub restart bricks the whole tool surface.** `aether-mcp` dials the hub once at startup and shares a single `Arc<RpcSession>` into every tool call — there is no re-dial. When the hub restarts (a rebuilt substrate, a wedged fleet) that session is a dead socket and every `mcp__aether-hub__*` tool fails until `aether-mcp` is itself restarted.
2. **`aether-mcp` is Claude's own transport.** Restarting it drops the MCP connection Claude is using — so "let Claude restart the MCP coordinator" is self-defeating: it would be sawing off the branch it sits on.

The common operational need is "restart the hub (and the substrate fleet it supervises) without losing the tool surface." The rarer need is restarting `aether-mcp` itself; its tool surface changes infrequently.

## Decision

Introduce a thin, long-lived **tunnel** process that Claude connects to and that supervises the volatile backends behind it. The tunnel is the stable MCP endpoint; the hub and `aether-mcp` become its supervised children.

- The tunnel binds the MCP port `.mcp.json` already targets (`:8890`) and **reverse-proxies** `/mcp` to `aether-mcp` on an internal port (`AETHER_MCP_PORT=8891`). The front Claude holds never restarts mid-session, so a backend restart never re-points the client. The proxy is a streaming pass-through: it does not interpret MCP, so an `aether-mcp` restart invalidates the downstream MCP session and Claude re-initialises (acceptable given how rarely `aether-mcp` restarts).
- The tunnel **forks and supervises** the hub (`AETHER_RPC_PORT=8901`) and `aether-mcp` (`AETHER_HUB_RPC_ADDR` pointed at the hub), restarting either on crash with backoff.
- `aether-mcp` is changed to **re-dial the hub on a dead session**. This — not the proxy — is what makes the common case (hub restart) seamless: the hub goes away and comes back, `aether-mcp` and its MCP session stay up, and the next tool call reconnects.
- Claude triggers a hub restart through an **out-of-band admin endpoint** on the tunnel (`POST :8890/admin/restart-hub`, `/status`), hit via a shell call. The `/mcp` proxy stays a dumb pass-through; management traffic does not ride the MCP channel.
- The tunnel is launched **on demand**: Claude runs the idempotent `scripts/ensure-tunnel.sh` (a no-op when `:8890` is already bound) at the point it needs the MCP harness. *(Originally auto-run by a `SessionStart` hook so neither Claude nor the maintainer hand-ran cargo; the hook was removed because a cold `cargo` build of the tunnel blocked session start long enough to look like a frozen session. The launch script stays — only the auto-trigger is gone.)*

`.mcp.json` is unchanged: `:8890` is the tunnel now instead of `aether-mcp` directly.

## Consequences

- Claude can restart the hub/substrate fleet on demand (admin endpoint) without losing its tools, and the stack is owned by a persistent supervisor that survives across Claude sessions rather than only while a session is alive.
- The re-dial fix is independently valuable and ships first (ahead of the tunnel): even with today's two-process layout, a hub restart stops bricking the tools.
- A new process sits in the MCP data path. The cost is a streaming reverse-proxy and a supervisor loop; the proxy stays MCP-agnostic, so it carries no schema knowledge and does not need to track the kind vocabulary.
- Restarting `aether-mcp` itself is *not* made seamless — its MCP session is dropped and Claude re-initialises. This is deliberate: hiding it would force an MCP-session-aware proxy, and `aether-mcp` rarely restarts. Revisit only if that assumption stops holding.
- Tracked in iamacoffeepot/aether#1212 (three PRs: the `aether-mcp` re-dial, the tunnel binary + admin endpoint, the bootstrap hook).

## Alternatives considered

- **Supervise-only, no proxy** (Claude connects to `aether-mcp` directly; the tunnel just forks/restarts both). Rejected as the primary form: restarting `aether-mcp` still blinks the transport, and the seamless-hub-restart win comes from the re-dial fix, not the supervisor — so a supervisor without a stable front buys little over a launch script.
- **Fold the hub into `aether-mcp`** (no separate tunnel; `aether-mcp` forks and supervises the hub). Fewer processes, but `aether-mcp` still needs an external starter and a restart of it still drops the transport — no stable front, and it entangles the coordinator with process supervision.
- **Injected management MCP tools** (`restart_hub` / `stack_status` as `mcp__` tools the tunnel merges into the proxied list). Cleaner ergonomically, but it forces the proxy to be MCP-aware (parse and merge tool lists, route management calls, forward the rest). Deferred behind the out-of-band admin endpoint; can be added later without changing the topology.
- **Plain launch script + `SessionStart` hook, no new process.** Lightest, but it does not solve hub-restart-without-blinking on its own (needs the re-dial fix regardless) and leaves no persistent supervisor — the stack dies with the session.
