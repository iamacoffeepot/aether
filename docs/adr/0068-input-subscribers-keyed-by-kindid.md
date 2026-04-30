# ADR-0068: Retire InputStream Enum; Key Input Subscribers by KindId

- **Status:** Proposed
- **Date:** 2026-04-30

## Context

The substrate currently carries two parallel identifiers for the same concept. Each platform-driven event source has both:

- a `Kind::ID` — the 8-byte `fnv1a_64` schema hash from ADR-0030 (`compile_time_kind_ids`), used by wasm components, the SDK, the mail wire, and dispatch routing;
- an `InputStream` enum variant (`Tick`, `Key`, `KeyRelease`, `MouseMove`, `MouseButton`, `WindowSize`) — used by the substrate's subscriber map (`HashMap<InputStream, BTreeSet<MailboxId>>`), the `SubscribeInput.stream` payload field, and the platform-thread fan-out path.

These are isomorphic by construction: there is exactly one `Kind` per `InputStream` variant, and they're populated together. But because they're stored independently, a hand-maintained translation table — `INPUT_STREAM_KINDS` in `crates/aether-substrate-core/src/control.rs` — bridges them. The table is six entries today; every new input kind would require an enum-variant addition, a table-row addition, and a fan-out call site in the platform thread.

`Kind::IS_INPUT` (a const bool emitted by `#[derive(Kind)]` when the `input` attribute is set) currently serves a documentation role: it tells you a kind is an input but carries no binding to a subscriber set.

The forcing function for this ADR is PR 404, which left `INPUT_STREAM_KINDS` in place as a deliberate temporary measure. The PR's body and the issue 405 acceptance criteria both treat the bridge table as load-bearing magic that should not survive long-term.

The ADR-0067 scenario harness shipped via PR 412 / issue 400, so refactor execution is no longer gated on missing test infrastructure (per the ongoing memory rule "scenario harness gates refactor execution, not filing"). End-to-end coverage already exists for the subscribe path: `input_subscription_yields_one_tick_observed_per_advance` and `drop_component_silences_tick_echoes` in `crates/aether-substrate-test-bench/tests/scenario.rs` exercise subscribe-fanout-drop end-to-end against a real wasm component fixture.

## Decision

Retire `InputStream`. Key everything by `KindId`.

Wire shape change on the control-plane mail kinds:

```rust
// before
pub struct SubscribeInput {
    pub stream: InputStream,
    pub mailbox: MailboxId,
}

pub struct UnsubscribeInput {
    pub stream: InputStream,
    pub mailbox: MailboxId,
}

// after
pub struct SubscribeInput {
    pub kind: KindId,
    pub mailbox: MailboxId,
}

pub struct UnsubscribeInput {
    pub kind: KindId,
    pub mailbox: MailboxId,
}
```

Substrate-side, the subscriber storage flips:

```rust
// before
input_subscribers: HashMap<InputStream, BTreeSet<MailboxId>>,

// after
input_subscribers: HashMap<KindId, BTreeSet<MailboxId>>,
```

Fan-out call sites in the desktop and headless chassis rewrite from `subscribers[InputStream::Tick]` to `subscribers[Tick::ID]`, sourced from the kind type's compile-time `<K as Kind>::ID`.

`Kind::IS_INPUT = true` becomes structurally load-bearing: it's the flag that means "this kind has a subscriber set." Adding a new input kind reduces to:

1. Define the kind type with `#[kind(name = "...", input)]`.
2. Emit events from the platform thread by sending `Mail::new(kind_subscribers[K::ID], K::ID, …)`.

No closed enum to extend. No bridge table to keep in sync. No new variant in `aether-kinds` for every input the platform layer learns to emit.

The SDK's `Ctx::subscribe_input::<K>()` body collapses to a single `send` of `K::ID`, since there is no longer a `TypeId` match step that picks the right `InputStream` variant.

The wire-shape change is documented here rather than rolled silently into the implementation PR because external tooling (hub, MCP harness, scenario YAML) serializes these payloads. Treating the rename and the field-shape change as one atomic step — with this ADR as the breadcrumb — keeps the migration legible.

### Wire size

`SubscribeInput` grows from ~9 bytes to ~16 bytes on the wire (1-byte enum varint → 8-byte `KindId`). Trivial in absolute terms but worth noting for any tool that pre-computes payload sizes.

### Migration breadth

Touchpoints, all in one PR:

- `aether-kinds`: remove `InputStream`, change `SubscribeInput` / `UnsubscribeInput` field shapes.
- `aether-substrate-core`:
  - `input::InputSubscribers` map type: `HashMap<InputStream, _>` → `HashMap<KindId, _>`.
  - Every fan-out call site (substrate platform thread for tick, key, mouse_*, window_size — `crates/aether-substrate-core/src/input.rs`, `crates/aether-substrate-desktop/src/chassis.rs`, `crates/aether-substrate-headless/src/chassis.rs`).
  - `handle_subscribe` / `handle_unsubscribe` in `control.rs` operate on `KindId`.
  - The bridge table `INPUT_STREAM_KINDS` and the `input_stream_for_kind_id` helper in `control.rs` are deleted.
  - Existing unit tests (`subscribe_adds_mailbox_to_stream_set`, `replace_component_preserves_subscriptions`, etc.) migrate to `KindId` keys.
- `aether-component`: SDK's `Ctx::subscribe_input::<K>()` simplifies to a one-line `send` of `K::ID`.
- Hub MCP surface: no rendering changes expected (`describe_kinds` / `engine_logs` don't surface `InputStream` directly), but a grep pass confirms.
- Scenario coverage: extend the test fixture and `tests/scenario.rs` to subscribe to a second input kind alongside Tick (e.g., Key) and verify per-kind fan-out routes correctly. Today's single-kind coverage doesn't exercise the multi-key map invariant — adding it locks the post-refactor shape.

## Consequences

### Positive

- One identifier space for inputs. The substrate, SDK, mail wire, and platform fan-out all key on the same 64-bit hash.
- Adding a new input kind is a one-line declaration plus the platform-thread emit. No enum variant, no bridge-table edit, no fan-out match arm.
- `Kind::IS_INPUT = true` becomes a load-bearing fact rather than a documentation hint. The compile-time const tells the substrate where to expect a subscriber set.
- Deletes `INPUT_STREAM_KINDS` and `input_stream_for_kind_id`, removing a synchronization burden the substrate authors have to remember to maintain.
- The post-migration code closely mirrors how every other `Kind`-keyed dispatch works in the substrate (the regular mail dispatch table is already `HashMap<KindId, _>`-shaped; inputs become a peer of that, not a sibling using a different key).

### Negative

- Wire-format change on `SubscribeInput` / `UnsubscribeInput`. Any external tooling that constructs these payloads by hand (e.g., scenario YAML, ad-hoc MCP `send_mail` calls, harness scripts) needs to update from `stream: "Tick"` to `kind: "test.tick"` (or whatever the kind's name resolves to). Slight friction during the migration window. No backward-compat shim is planned — both ends update atomically in one PR.
- `SubscribeInput` grows by 7 bytes on the wire. Negligible.
- `IS_INPUT = false` on a kind that's mistakenly subscribed-to no longer fails at compile time via the closed `InputStream` enum. The substrate handles this as a no-op subscribe (mailbox key just never receives anything because the platform thread doesn't fan out to non-input kinds), which is the same outcome as before in practice but is now caught at runtime rather than at the type system.

### Neutral

- The `Kind::IS_INPUT` const stays. Its semantics tighten from "this kind is an input" to "this kind has a subscriber set the substrate maintains."
- The `HashMap` choice carries over. If subscriber-set lookup ever shows up in profiles, switching to a small fixed-size array indexed by a per-kind index is a follow-on optimization that doesn't depend on this ADR.
- Auto-subscription on component init (the SDK's `#[handlers]`-driven walker that sends `subscribe_input` for every `K::IS_INPUT` handler kind) is unchanged in shape — only the field name in the emitted mail flips.

## Alternatives considered

### Keep `InputStream`; add the missing variants on demand

Status quo. Status quo is a paper cut every time a new platform input source is added: extend the enum, extend the bridge table, extend the fan-out match. The dual-identifier dance survives forever.

Rejected: the work to add a new input today is structurally larger than the work this ADR's post-migration model requires.

### Make `InputStream` derived from `Kind` automatically

Have the `Kind` derive emit a per-kind `InputStream` value when `input` is set, then have `SubscribeInput.stream: InputStream` carry it. The substrate keeps the `HashMap<InputStream, _>` shape but the bridge table is auto-generated.

Rejected: still maintains two identifier spaces (`InputStream` and `KindId`), and the auto-derive adds proc-macro complexity for no end-state cleanup. The whole point of `KindId` is that it already serves as the universal identifier — adding a parallel auto-derived enum is a sideways move.

### Use `TypeId` rather than `KindId`

`std::any::TypeId` is a host-side native identifier; works only in native code. Components run as wasm; they can't construct a `TypeId` for the host's types. `KindId` is precisely the cross-boundary identifier that already works.

Rejected: shape mismatch with the rest of the system.

### Stage the migration over two PRs

Step 1: add `kind: KindId` to `SubscribeInput` alongside `stream: InputStream`, deprecate the old field, ship. Step 2: drop `stream`. Lets external tooling migrate at its own pace.

Rejected: the workspace is the only consumer of this wire shape today (all components, scenarios, and tooling live in this repo), so the staged migration buys nothing and adds a window where both fields exist and could disagree. One atomic PR with the ADR for context is cleaner.
