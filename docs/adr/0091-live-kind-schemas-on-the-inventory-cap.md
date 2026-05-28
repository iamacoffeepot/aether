# ADR-0091: Live kind schemas on the inventory cap

- **Status:** Proposed
- **Date:** 2026-05-28

## Context

The MCP harness can't encode mail for kinds a loaded component defines. Concretely, the "draw a cube" sequence — `load_component aether-mesh-viewer`, then `send_mail aether.mesh.load …` — fails with `unknown kind` because `aether-mesh.load` was never in the harness's encode lookup. The encode path in `aether-mcp/src/tools.rs` (the `descriptors` snapshot fed into `aether_codec::encode_schema` at `:230, :269, :510, :1196, :1211`) is `aether_kinds::descriptors::all()` — the static substrate vocabulary that `aether-mcp` links against directly. Component-defined kinds never enter it. This blocks the core "Claude drives a loaded component" use case (issue #1232).

Worth correcting a piece of the issue's framing: the schemas **do** ship in the wasm. `aether-actor-derive` emits a `#[unsafe(link_section = "aether.kinds")]` static carrying the canonical `SchemaType` bytes (`aether-actor-derive/src/lib.rs:213`, ADR-0028 / ADR-0032 / issue 640). On load, `ComponentHostCapability::handle_load` parses that section via `kind_manifest::read_from_bytes` and calls `register_or_match_all` (`crates/aether-capabilities/src/component.rs:242–248`), registering every kind — with full schema — into the substrate's `Registry`. The schemas reach the substrate. What they don't reach is the harness, which is a separate process. The two stale-looking pieces of wire that *could* have carried them — `PeerKind::Substrate.kinds` on the `Hello` handshake (`crates/aether-capabilities/src/rpc/wire.rs:94–105`) and `HandlerCapability` inside `ComponentCapabilities` (`crates/aether-kinds/src/lib.rs:1088–1092`) — are both name-only by design: the former is sent as `vec![]` at both ends today; the latter ships `{id, name, doc}` for `describe_component` rendering and was never meant to carry schemas.

So the actual gap is small: the substrate already has every schema; the harness has no way to read them across the RPC.

There is already an actor whose job is "what's in this engine right now": `aether.inventory` (ADR-0088 §6, `crates/aether-capabilities/src/inventory.rs`). Today it serves `Manifest` (compile-time name inventory) and `Resolve` (per-id reverse lookup). Both are read-through to process-global tables, no actor state. The cap is chassis-owned and on both the desktop and headless chassis. It is a poll-shaped surface by construction — clients drive it, the cap holds nothing.

The user has stated a clear preference here: polling over push. ADR-0088 already established the inventory cap as the polling actor. The mailbox-side counterpart (`MailboxesChanged`, issue #730) uses push for the analogous problem on mailboxes — that's the path we explicitly do not want to retake.

## Decision

Widen `aether.inventory`'s role from compile-time reverse-lookup to **live per-engine registry view**, and adopt **lazy-on-miss** caching on the harness side.

### 1. One new request kind on `aether.inventory`

Add a third request to the cap, parallel to `Manifest` and `Resolve`:

```rust
#[kind(name = "aether.inventory.kinds")]
pub struct ListKinds {}

#[kind(name = "aether.inventory.kinds_result")]
pub struct ListKindsResult {
    pub kinds: Vec<KindDescriptorWire>,
}

pub struct KindDescriptorWire {
    pub id: KindId,
    pub name: String,
    pub schema: SchemaType,
}
```

`ListKinds` is the empty request; the call itself is the signal, mirroring `Manifest`. The reply is the substrate's authoritative current vocabulary: every `KindId` registered in the engine's `Registry`, with its full `SchemaType`.

### 2. The cap reads the live `Arc<Registry>`

The handler implementation is the projection of `Registry::list_kind_descriptors() -> Vec<KindDescriptor>` (already defined on `aether-substrate/src/mail/registry.rs:1091`) onto `Vec<KindDescriptorWire>`. The cap stops being stateless: in `init` it pulls `Arc<Registry>` via `NativeInitCtx::mailer().registry()` — the same `Arc` `ComponentHostCapability` clones at `component.rs:170`.

The propagation story falls out of this for free. `ComponentHostCapability::handle_load` mutates the shared `Arc<Registry>` via `register_or_match_all` (`component.rs:246`); the inventory cap's handler reads that same `Arc` on every call. Registrations and removals are visible to the inventory cap the moment they return. No event channel, no notification kind, no cache invalidation. The shared `Arc` is the propagation.

### 3. Lazy-on-miss caching in `aether-mcp`

The harness keeps a per-engine `HashMap<String, KindDescriptor>` keyed by kind name. The encode path becomes:

```
encode(engine, kind_name, params):
  1. lookup cache(engine).get(kind_name)
        → hit  → encode_schema(params, schema), done
        → miss → step 2
  2. refresh(engine): RPC ListKinds, replace cache(engine) with the reply
  3. retry lookup
        → hit  → encode, done
        → miss → return error "unknown kind: {kind_name}"
```

Refreshes collapse: a per-engine async mutex around the refresh ensures two concurrent encode misses on different unknown names trigger one RPC, not two. The second waiter awaits the first's result and proceeds to step 3 without re-fetching.

The static `descriptors::all()` snapshot stays as the boot-time prefill (each engine's cache starts populated with the substrate's known vocab) and as the fallback for paths that don't have an engine context (`describe_kinds` returns the static set unchanged). The new per-engine cache is layered on top, not a replacement.

### 4. Cache lifetime

Cache entries live for the harness session. There is no TTL, no background refresh, no scheduled poll. Entries are added on miss-refresh and on `LoadResult` Ok-path (a free signal the harness already receives — see Consequences). Entries are not actively removed; a `replace_component` that shrinks the vocab leaves stale entries that no longer dispatch, and that's the substrate's rejection to surface, not the harness's cache to police (see Consequences).

### 5. ADR-0088 §6 scope widens

ADR-0088 §6 framed the inventory cap as "reverse-lookup id ↔ name." This ADR widens that to "live per-engine registry view" — same per-build authority, same chassis ownership, three handlers instead of two. `Manifest` and `Resolve` are unchanged; `ListKinds` joins them. The §6 docstring on the cap (`crates/aether-capabilities/src/inventory.rs:1–28`) updates to reflect the broader role.

## Consequences

**The forcing-function use case unblocks.** After this ADR ships, `load_component aether-mesh-viewer` followed by `send_mail aether.mesh.load …` encodes correctly through the harness, and "draw a cube" works end-to-end. So does any component whose kind crate isn't hand-promoted into `aether-kinds`. The hand-promotion precedent set by `aether.mesh.load_result` (iamacoffeepot/aether#964) stops being a per-kind ritual.

**No ABI change.** The wasm `aether.kinds` custom section, `ComponentCapabilities`, the `#[actor]` / `export!` codegen, `LoadResult`, and the `Hello.kinds` wire slot all stay as-is. The change is scoped to: one new request/reply kind pair in `aether-kinds`, one new handler on `inventory.rs`, the harness-side cache + refresh, and a `Cargo.lock` bump on `aether-mcp`.

**Zero idle cost.** No timer, no background poll, no chassis-side push to multiplex. The cap's handler is read-only over `Arc<Registry>`; the cache lives only in the harness process and only mutates on miss-refresh.

**One-time post-load latency.** The first `send_mail` against a freshly-registered kind pays one extra RPC roundtrip (a `ListKinds` call against the engine's `aether.inventory` mailbox). Single-digit ms over loopback. Acceptable for harness pacing — the harness is human-paced or LLM-paced, not the hot path. The harness can also opportunistically refresh on `LoadResult` Ok-path so the very first post-load `send_mail` hits the cache, but this is a "nice to have" optimization, not load-bearing.

**`replace_component` shrink leaves stale entries.** If a `replace_component` replaces a wasm whose vocabulary was `{A, B, C}` with one whose vocabulary is `{A, B}`, the harness still has `C` in its cache. `send_mail C` encodes successfully but the substrate rejects on dispatch (the registry no longer holds `C`). That rejection is the safety net; the harness sees an explicit error, the agent retries with a current vocab if needed. We do not invalidate cache entries proactively because the only correct invalidation would come from push notification on shrink, which is exactly the architectural shape this ADR rejects. Concretely: `replace_component` is rare, vocab-shrinking replacements rarer, and the substrate's response distinguishes "your cache is stale" from "this kind never existed" by error type. Not a correctness bug.

**Inventory cap stops being stateless.** Today the cap reads link-time consts and has no state. With this change it holds `Arc<Registry>` (cheap, the chassis already passes that `Arc` to several caps). The `#[bridge(singleton)]` shape is preserved; the new field is the only delta to the cap's storage.

**Encode path widens.** The four sites at `tools.rs:230, :269, :510, :1196, :1211` change from "find in static descriptor list" to "find in per-engine merged view (static + cache)." A hashmap lookup, still O(1). The static `descriptors::all()` snapshot stays referenced as the cache prefill.

**aether-mcp gains a runtime RPC dependency on `aether.inventory`.** Both the desktop and headless chassis already include the cap (`with_common_caps`), and the substrate-bundle's `test_bench` reuses those builders, so this is uniformly present. A future chassis variant that omits the inventory cap would not work with this harness path — worth flagging as a chassis-build invariant if a stripped variant is ever proposed.

**ADR-0088 §6 scope widens, §3/§4/§5 unchanged.** Manifest and Resolve continue to serve names; the cap now also serves kind schemas. The "per-build authority, chassis-owned, polled by clients" model is unchanged, just applied to one more axis.

**Out of scope for this ADR / follow-up tracking.** The promote-to-native stopgap for `aether.mesh.load` noted in #1232 (hand-promote the request kind alongside its already-promoted `load_result`) can land independently to unblock cube-drawing before this ADR's implementation ships; track separately if wanted. This ADR is not the place to fold it in — it's a band-aid that doesn't generalize, and the generalization is precisely what this ADR is.

## Alternatives considered

- **Push on registration — emit a `KindsChanged` mail when `load_component` / `drop` / `replace` mutates the registry.** Architecturally symmetric to `MailboxesChanged` (#730) and would eliminate even the one-time post-load latency. Rejected on user preference: the harness is the actuator for all current `load_component` calls, so it knows causally when the vocab changed; push is paying ongoing structural cost (a notification mailbox, fan-out, ordering across hub-RPC) to save one RPC of latency the harness wouldn't notice. The only out-of-band loader path is a hypothetical chassis-internal actor — none exist today, and lazy-on-miss covers that case for free when it lands.

- **Periodic polling on a timer.** Simple to describe, but pays a continuous RPC cost to shave a one-time latency hit, introduces a knob to tune (poll interval), and *still* misses up to one interval's worth of newly-loaded kinds between ticks. Rejected: worst-of-both shape — neither as cheap as lazy-on-miss nor as responsive as push.

- **Ride `LoadResult` — include the loaded component's full descriptors in the reply.** Cheapest possible (no new RPC, no new request kind), but covers only the component's own kinds. It doesn't cover the substrate's static vocab (which would still need `Hello.kinds` populated or a one-time `describe_kinds`), doesn't cover chassis-internal future loaders, and couples harness encode-correctness to a wasm-load lifecycle event. Rejected as a primary mechanism; can still be done as an opportunistic prefill on top of the lazy-on-miss path if measurement shows the first-send latency is a problem.

- **A new `aether.schemas` cap, separate from `aether.inventory`.** Same lifecycle, same read source, same chassis ownership, same client. Splitting them buys one extra namespace for the polling client to remember and one extra cap to wire into chassis builders. Rejected: the two are answering the same shape of question ("what's in this engine right now") off the same authority.

- **Promote each component kind into `aether-kinds` (the stopgap noted in #1232).** Works for one or two kinds, fights ADR-0066 ("kinds live with the component") for any quantity, and turns "ship a new component" into "edit `aether-kinds`." Rejected as a strategy; the targeted promotion of `aether.mesh.load` to unblock cube-drawing in the short term is a separate, independently-trackable workaround.

## References

- Issue #1232 (forcing function — needs an ADR before code)
- ADR-0028 (kind manifest custom section), ADR-0032 (the `aether.kinds` v0x04 schema-bytes section)
- ADR-0033 (component receive-side capability surface)
- ADR-0064 (tagged-id strings — wire form of `KindId` on the MCP boundary)
- ADR-0066 (kinds live with the component)
- ADR-0088 (reverse-lookup identifier inventory — the cap this ADR widens)
- iamacoffeepot/aether#964 (the `aether.mesh.load_result` promote-to-native precedent)
- iamacoffeepot/aether#730 (`MailboxesChanged` — the push-shaped counterpart this ADR explicitly does not retake)
