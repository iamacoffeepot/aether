# ADR-0083: Mail sender vs origin — naming the lineage, retiring `ReplyTo`

- **Status:** Proposed
- **Date:** 2026-05-19
- **Builds on:** ADR-0013 / ADR-0017 (reply_mail), ADR-0037 (cross-engine bubble), ADR-0042 (correlation ids), ADR-0080 (mail tracing + settlement lineage)

## Context

Mail in aether is *pushed* at a recipient — there is no "from" address; a mail carries a reply-routing hint instead (`crates/aether-data/src/mail.rs:88`). That hint is today called `ReplyTo`, and the name is actively misleading on two counts.

**1. The name imports a semantic the code rejects.** "Reply-to" connotes a *retained, freely-set, end-to-end redirect* — point replies at a chosen target and have that target persist down the chain. aether's field is the opposite: it is the **immediate sender**, auto-stamped to the *sending actor's own mailbox* on every send (`crates/aether-substrate/src/actor/wasm/component.rs:200`, `crates/aether-substrate/src/actor/native/binding.rs:408`), and there is no component-facing API to set it to an arbitrary target. In a chain A→B→C, when B sends to C the substrate stamps `Component(B)`, not `Component(A)` — so C's reply routes to B, and the reply target *changes every hop*. A reader who trusts the name reasonably expects retention and is surprised it doesn't persist. (This ADR exists because that surprise actually happened.)

**2. Two distinct types share the name.** The wire struct `aether-data::mail::ReplyTo { target: ReplyTarget, correlation_id }` (`mail.rs:123`) and the guest-side opaque `u32` handle `aether-actor::mail::ReplyTo` (`crates/aether-actor/src/mail/mod.rs:53`) are entirely different things at different layers, both named `ReplyTo`.

**The lineage actually has two levels, and aether already tracks both — just split across layers.** The *immediate sender* lives in the **addressing** layer (today's `ReplyTo`/`ReplyTarget`). The *chain origin / root* lives in the **tracing** layer — ADR-0080 stamps `root` (chain origin) and `parent_mail` (immediate causal parent) on every outbound (`crates/aether-substrate/src/actor/native/ctx.rs:314-328`). Nothing names this split, so the addressing field gets mistaken for the origin.

## Decision

A pure naming + model-documentation change. No behavior changes.

### 1. Rename the wire type to `Sender`

| Today (`aether-data`) | Renamed |
|---|---|
| `ReplyTo { target, correlation_id }` | `Sender { addr, correlation_id }` |
| `ReplyTarget` (`None` / `Session` / `EngineMailbox` / `Component`) | `SenderAddr` (same variants) |
| `ReplyTo::NONE` / `::to(..)` / `::with_correlation(..)` | `Sender::NONE` / `::to(..)` / `::with_correlation(..)` |

`Sender` names the *relationship* ("who sent this"); replying is the derived use, not the identity. `SenderAddr::None` reads correctly for broadcast / system mail (no identifiable sender). Field/constructor renames follow mechanically.

### 2. Rename the guest handle to `ReplyHandle`

| Today (`aether-actor`) | Renamed |
|---|---|
| `ReplyTo` (opaque `u32`) | `ReplyHandle` |
| `Mail::reply_to() -> Option<ReplyTo>` | `Mail::reply_handle() -> Option<ReplyHandle>` |
| `Ctx::reply(handle, ..)` | unchanged |

The guest's "reply" verb is correct — from a component's seat, the use *is* replying, and the handle *is* a reply capability. The rename only removes the collision with the wire type. The substrate-side `ReplyEntry` (`reply_table.rs`) stays on the reply side; its `target` field renames to `addr: SenderAddr` for consistency with §1.

### 3. Codify the sender/origin lineage split

State it once, in the types' doc comments and here:

- **Addressing layer — `Sender`.** The *immediate* sender. One hop. Re-stamped to the sending actor's own mailbox on every send; auto-bound, never an arbitrary target. This is what a reply routes to.
- **Tracing layer — `root` + `parent_mail` (ADR-0080).** `root` is the chain *origin*; `parent_mail` is the immediate causal parent. The full lineage is observable here.

`Sender` is not the origin. The thing a reader imagines "persists through the chain" is the origin, and it does persist — in tracing, not addressing. Addressing is deliberately one-hop; the chain origin is observable, not addressable.

### 4. Addressing stays one-hop; "message the sender" is not a capability

A `ReplyHandle` is an **unforgeable, one-shot reply capability** to a specific origin: the guest receives an opaque handle (not a `MailboxId`), can reply to any correspondent it has actually received from (handles are stashable for the instance lifetime), but cannot fabricate a target or address a mailbox that never wrote to it. Replies themselves carry `Sender::None` (terminal — "nobody replies to a reply", `crates/aether-substrate/src/mail/mailer.rs:383`).

There is intentionally **no reusable sender address** exposed to components and **no API to address the chain origin**. A component that wants an ongoing two-way exchange must have the peer's `MailboxId` as *data* (in the payload), not derive it from the inbound. Deep "result flows back to the originator" pipelines use the **DAG executor** (observers deliver terminal handles to named recipients), not reply-chain-walking — which per-hop addressing structurally would not support anyway. Adding a reusable sender-address, or origin-addressing, is a separate and deliberate capability decision; it is out of scope here precisely because it re-opens the cross-chain-redirect surface this model avoids.

### 5. No wire or ABI change

This is a source rename only. Postcard / cast encoding is structural (by field layout, not type name); the guest handle's `u32` value is unchanged. Wire bytes and the `_p32` FFI ABI are byte-identical before and after. Only Rust source referencing the names changes.

## Consequences

### Positive
- The name matches the semantics: `Sender` says "immediate, changes per hop," instead of `ReplyTo` falsely promising a retained redirect.
- The two-types-one-name collision is gone (`Sender` wire-side, `ReplyHandle` guest-side).
- The sender-vs-origin lineage split is named and discoverable, mapping onto aether's addressing/tracing layers.

### Negative
- Mechanical rename across the `ReplyTo` / `ReplyTarget` surface (~20 files). Best done in one pass via an IDE rename refactor, not hand edits.
- The guest SDK rename (`ReplyTo` → `ReplyHandle`, `reply_to()` → `reply_handle()`) is a source-breaking change for component authors. Pre-1.0, acceptable; components recompile, runtime behavior is unchanged.

### Neutral
- Zero behavior change. This ADR is naming plus a model statement; nothing routes differently.

## Alternatives considered

- **`Source` instead of `Sender`.** Equally clear. `Sender` chosen because it names the relationship directly — the immediate sender — and keeps `origin` / `root` as the distinct tracing vocabulary, so "sender = immediate, origin = chain root" lands without conflating the two.
- **Keep `ReplyTo`, just document it.** Rejected — the name actively mis-signals a retained redirect. A doc fixes one reader; the name keeps misleading the next.
- **Promote `origin` into addressing** (let a node address the chain root directly). Rejected — re-enables cross-chain reply redirects, the abuse surface this model deliberately avoids. Origin stays in tracing: observable, not addressable.

## Migration

One mechanical rename PR (IDE rename refactor over the wire `ReplyTo`/`ReplyTarget` and guest `ReplyTo` surface). Fold in the stale `Mail::reply_to()` doc fix: its rustdoc says component-to-component mail has no reply handle, but `deliver()` allocates one for `Component`-origin mail (`crates/aether-substrate/src/actor/wasm/component.rs:536-538`). Rebuild wasm components (source recompile; ABI unchanged). No wire migration, no data migration.
