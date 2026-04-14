# ADR-0011: Origin attribution on observation mail

- **Status:** Accepted
- **Date:** 2026-04-14

## Context

ADR-0008 shipped the engine→Claude observation path. Every broadcast carries `engine_id`, `kind_name`, `payload`, and a `broadcast` flag. Today this is enough: each substrate hosts one component, so `engine_id` and `kind_name` together unambiguously identify what emitted a broadcast.

ADR-0010 breaks that assumption. A multi-component substrate can have two components emitting the same kind — e.g., a `"physics"` and a `"render"` component both pushing `aether.observation.frame_stats`. With today's wire, Claude sees two broadcasts from the same engine with the same kind and cannot tell them apart without peeking at payload details. That's a regression in observability right at the moment multi-component becomes interesting.

The current substrate also emits broadcasts from the main thread (the frame loop pushes `FrameStats` every 120 frames). Those pushes aren't from a component — they have no sending mailbox. Any attribution scheme has to be honest about that.

Forces at play:

- **Attribution should be cheap.** Minting a named sender identity for every substrate-internal emitter just to keep a field populated is plumbing cost without present benefit. There's only one substrate-side non-component emitter today and no roadmap item that requires distinguishing several.
- **A `None` is more truthful than a placeholder.** Calling the main-thread push `"substrate"` or `"core"` invents an identity that doesn't correspond to anything in the registry. The honest answer is "there's no mailbox sender here."
- **The hub shouldn't interpret the field.** Origin is substrate-attested. The hub translates, routes, and forwards; the meaning of the name lives on the substrate side.
- **The extension must be additive.** ADR-0008's `EngineMailFrame` is deployed. Consumers that don't read origin should keep working; new consumers get the new field.

## Decision

Add an optional origin identifier to broadcast observation mail, carried end-to-end from the substrate through the hub to the MCP `receive_mail` surface.

### 1. Wire format

Extend `EngineMailFrame` in `aether-hub-protocol`:

```rust
pub struct EngineMailFrame {
    pub address: ClaudeAddress,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub origin: Option<String>, // NEW — substrate-local mailbox name of the emitter, if any
}
```

`ReceivedMail` on the MCP tool output grows a matching `origin: Option<String>`. Postcard (de)serialization of `Option<String>` is a single length-prefix byte for `None`; the wire cost when absent is one byte.

### 2. Substrate responsibility

When a component emits to the `hub.claude.broadcast` sink, the substrate resolves the sending mailbox's registered name from the `Registry` and stamps it as `origin`. When the push originates from substrate core (e.g., `main.rs`'s frame-loop `FrameStats` emission, or any future substrate-owned workflow), `origin` is `None`.

Concretely: the broadcast sink handler grows access to the sending mailbox identity (either via the sink handler signature, or via `Mail` growing a `sender: Option<MailboxId>` field — implementation choice deferred). The handler looks up the name and constructs the frame.

### 3. Hub responsibility

Transparent passthrough. The hub does not validate, normalize, or interpret `origin`. It's substrate-attested — if a substrate lies about it, the hub forwards the lie.

## Consequences

### Positive

- **Resolves the multi-component ambiguity from ADR-0010.** Two components emitting the same kind are distinguishable by name at the MCP layer, without peeking at payload.
- **Additive to ADR-0008.** Existing consumers that don't read `origin` keep working; new consumers get the new field.
- **No new registry machinery.** The substrate already knows every mailbox's name. Attribution is lookup, not invention.

### Negative

- **Claude-side consumers have to handle `None`.** Every place that reads `origin` gets an `Option` branch. For a polling agent that displays origin as a label, that's a small cost; for anything that joins on origin, it's a real edge case.
- **`origin` is untrusted.** A buggy or malicious substrate can set it to anything. Fine under the single-tenant V0 assumption; a multi-tenant future needs either signed attribution or a hub-enforced mapping.
- **Sink handler signature churn.** Touching the broadcast sink's calling convention ripples through any other sink that takes the same shape. Small blast radius today (render sink, broadcast sink), worth naming before that surface multiplies.

### Neutral

- **Upgrade to always-populated is trivial.** If substrate-side emitters proliferate and the `None` branch starts to hurt, registering substrate core as a named sender is additive — `Option<String>` keeps working, and components populated today continue to populate.
- **The field is a name, not an id.** `MailboxId` is substrate-local and not meaningful to the hub or Claude. Names are stable across spawn cycles if the substrate registers them the same way.

## Alternatives considered

- **Register substrate core as a named sender.** Always-populated `String` with a reserved name like `"aether.substrate"`. Rejected on plumbing cost: mint a reserved name, protect it from component collision, thread the sender through `Mail` and the scheduler so main-thread pushes stamp it. Not worth the cost for a single non-component emitter. If pressure surfaces (more substrate-owned emitters, or a desire for a single uniform contract), the upgrade is additive.
- **Encode origin in `kind_name`.** E.g., `"physics.aether.observation.frame_stats"`. Rejected: conflates kind vocabulary with component identity, duplicates data that the registry already owns, and forces components emitting shared kinds to lie about the kind they're sending.
- **Use `MailboxId` on the wire instead of name.** Rejected: ids are substrate-local and change across substrate restarts. Names are the stable identity that makes cross-session reasoning possible.
- **Infer origin from a per-component observation kind namespace.** Convention: components prefix their emitted kinds with their name. Rejected: convention, not enforceable; components emitting shared kinds (the exact case that motivated this ADR) can't follow it.

## Follow-up work

- `origin: Option<String>` on `EngineMailFrame` in `aether-hub-protocol`. Wire change, but ADR-0008 is V0 and has no external consumers beyond this repo.
- Substrate: broadcast sink resolves sending mailbox name via the `Registry`. main.rs's direct pushes pass `None`. The exact mechanism (sink handler signature change vs. `Mail::sender`) is a PR-time decision.
- Hub: propagate `origin` from `EngineMailFrame` to `ReceivedMail` on the MCP tool output.
- **Parked, not committed:** always-populated substrate identity, signed/trusted attribution, per-kind origin policy (e.g. "this kind must have origin"), cross-substrate correlation.
