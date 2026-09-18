# Replacement failure states

`replace_component` now prepares a successor while the old guest remains usable. A `ReplaceResult::Err` means the preparation was rejected: the old guest, its stable mailbox and inline-child routes, and its registered capabilities remain in place. If the slot was already empty after `drop_component`, it remains empty. Read [Component registry](../component-registry.md) for normal load, replace, and drop behavior.

| Phase | Result on failure |
|---|---|
| Module compile, manifest, export, or asset validation | No guest lifecycle hook runs; prior slot and descriptions stay unchanged. |
| Read-only `on_dehydrate` | A trap, nonzero status, forbidden effect, or failed state save rejects preparation while the predecessor is still wired. |
| Candidate `init` or `on_rehydrate` | The private candidate and its outbound mail, replies, and log events are discarded. The predecessor continues to answer at its original address. |
| Candidate topology | New aliases, alias retirements, and detached sibling births requested during preparation are rejected until owner admission can be guaranteed atomically. Existing inline aliases survive a successful reconstruction. |
| Accepted replacement | Old `unwire` runs, then the successor, module metadata, capabilities, and cost cells become current. Buffered candidate effects are released once. |

The same `on_dehydrate`/`save_state` ABI captures state before old `unwire`. The SDK exposes an immutable actor borrow and a persistence-only context for this preparation step. Guest authors must also avoid logical mutation through interior mutability or raw Wasm; the host can block imported effects but cannot prove every instruction is pure. A saved bundle requires a working restore export. Required inline-child reconstruction failures reject the candidate. An intentional typed-state schema mismatch handled by the guest's migration policy can still boot fresh under [ADR-0113](../../../adr/0113-kind-typed-actor-state.md).

An `unwire` trap after acceptance is logged and contained. It does not turn an accepted replacement into an error that suggests rollback. Native DROP and REPLACE restrictions continue to apply to the slot, including an empty slot after a permitted drop.

To investigate a rejected replacement, keep the returned error together with the requested content hash, export, config, and mailbox id. A harmless application query can confirm the predecessor's state. `describe_component` is useful for capability comparison, but the MCP process can return a cached description; use a live query when liveness matters. An empty slot can be refilled through `replace_component` when replacement is allowed.

The same-handler result covers one component replacement. It is not an atomic commit with a separate journal append or other external transaction; that boundary requires its own prepare/commit coordination.

## Source routes

- Transaction preparation and commit: `crates/aether-component/src/trampoline/runtime/replace.rs`
- Candidate effect capture: `crates/aether-substrate/src/actor/wasm/component/ctx.rs`
- Guest restoration status: `crates/aether-substrate/src/actor/wasm/component/lifecycle.rs`
- MCP replace result and cache refresh: `crates/aether-mcp/src/tools/components.rs`
