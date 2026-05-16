# Stale symbol catalogue

Identifiers that retired or renamed and shouldn't appear as live references anymore. Used by `/audit stale <crate>`. The audit asks the resolver first (`search_symbol`) — a hit there means someone reintroduced the identifier and the finding is high-severity. Only when the resolver returns nothing does the audit fall through to a text search for in-comment / in-string mentions (medium-severity stale comment).

**Append-only.** When you retire a new symbol, add an entry here in the same PR. Mark deprecated entries inline with `status: removed-from-catalogue — <reason>` rather than deleting (older audit reports may still cite the entry).

Entry shape:

```
- `<symbol>` | <retirement PR / issue> | <successor or `removed`> | <optional severity override>
```

`<symbol>` is the bare identifier the resolver / text search looks for. `<retirement PR>` is the cross-repo form (`iamacoffeepot/aether#NNN`) so the finding body can link back. `<successor>` is the kind name, type, or const that replaced it (or the literal word `removed` if nothing replaced it). The optional severity column overrides the default mapping (live ref = high, comment-only = medium).

---

## Retired in 2026

### Crates / modules

- `aether-id` | iamacoffeepot/aether#444 | `aether-data` typed-id newtypes
- `aether-mail` | iamacoffeepot/aether#444 | split between `aether-component` (SDK) and `aether-substrate` (dispatcher)
- `aether-params-codec` | iamacoffeepot/aether#444 | `aether-codec`
- `aether-hub-protocol` | iamacoffeepot/aether#444 | `aether-substrate-bundle::hub::wire`
- `aether-substrate-core` | ADR-0073 | `aether-substrate`
- `aether-substrate-desktop` | ADR-0073 | `aether-substrate-bundle` `desktop` submodule
- `aether-substrate-headless` | ADR-0073 | `aether-substrate-bundle` `headless` submodule
- `aether-substrate-hub` | ADR-0073 | `aether-substrate-bundle` `hub` submodule (the binary name `aether-substrate-hub` is preserved as the binary entry point)
- `aether-substrate-test-bench` | ADR-0073 | `aether-substrate-bundle` `test_bench` submodule
- `aether-demo-tic-tac-toe` | iamacoffeepot/aether#782 | `removed`
- `aether-demo-tic-tac-toe-server` | iamacoffeepot/aether#782 | `removed`
- `aether-demo-tic-tac-toe-client` | iamacoffeepot/aether#782 | `removed`

### Caps + types

- `BroadcastCapability` | iamacoffeepot/aether#778 | `removed`
- `HubClient` | iamacoffeepot/aether#777 | `removed` (substrate-side; out-of-process `aether-mcp` dials the hub RPC server directly)
- `connect_hub_client` | iamacoffeepot/aether#777 | `removed`
- `ProcessCapability` | iamacoffeepot/aether#773 | `removed`

### Kinds + mailbox names

- `HUB_BROADCAST_MAILBOX_NAME` | iamacoffeepot/aether#778 | `removed`
- `aether.observation.frame_stats` | iamacoffeepot/aether#778 | `removed`
- `aether.observation.component_died` | iamacoffeepot/aether#778 | `removed`
- `aether.observation.substrate_dying` | iamacoffeepot/aether#778 | `removed`
- `aether.observation.monitor_notice` | iamacoffeepot/aether#780 | `aether.actor.monitor_notice`
- `aether.sink.` (any kind name with this prefix) | ADR-0074 Phase 5 | `aether.<name>` (e.g. `aether.sink.audio` → `aether.audio`)
- `tic_tac_toe.play_move` | iamacoffeepot/aether#782 | `removed`
- `tic_tac_toe.reset` | iamacoffeepot/aether#782 | `removed`
- `tic_tac_toe.game_state` | iamacoffeepot/aether#782 | `removed`
- `tic_tac_toe.move_result` | iamacoffeepot/aether#782 | `removed`

### APIs / FFI

- `resolve_kind_p32` | ADR-0030 Phase 2 | `Kind::ID` compile-time const
- `resolve_mailbox_p32` | ADR-0029 | `mailbox_id_from_name(<Cap>::NAMESPACE)` compile-time const
- `Component::receive` (typelist dispatch) | ADR-0033 | `#[handlers]` macro + per-kind `#[handler] fn ...`
- `type Kinds = ...` (in `Component` impls) | ADR-0033 | retired — handlers are the source of truth
- `mailboxes::*` typed registry module | iamacoffeepot/aether#613 | inline `mailbox_id_from_name(<Cap>::NAMESPACE)` at use sites
- `attach_component_for_test` | issue 648 | route through real load + advance via `TestBench`
- `ComponentHostCapability::for_test` | issue 648 | same
- `EngineToHub` (engine-side listener) | iamacoffeepot/aether#773 | the hub is now a thin chassis hosting `RpcServerCapability` + `EngineServer` directly

## Format extensions

If a future entry needs more structured metadata (e.g. "warn even in comments because the retirement is sensitive"), extend the shape additively rather than re-encoding existing entries. The audit's parsing tolerates extra `|`-separated columns.
