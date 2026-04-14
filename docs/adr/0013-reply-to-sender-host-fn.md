# ADR-0013: Reply-to-sender host fn

- **Status:** Proposed
- **Date:** 2026-04-14

## Context

ADR-0008 shipped engine→Claude mail with two addressing modes on the wire: `ClaudeAddress::Broadcast` and `ClaudeAddress::Session(SessionToken)`. The broadcast path is exposed today — guests send to the reserved `"hub.claude.broadcast"` mailbox and the hub fans out. The session-token path is plumbed end-to-end on the wire but the **host fn** that lets a WASM component send one is still missing.

Concretely, today's flow:

- Hub → engine frames carry a `sender: SessionToken` on every `HubToEngine::Mail`.
- The substrate's receive path has the token (it comes in on the wire).
- The guest-facing `receive(kind, ptr, count)` shim does not pass it through.
- The guest-facing `send_mail(recipient, kind, ptr, len, count)` only addresses mailboxes by `MailboxId` — there's no way to name a session.

Result: a component can observe-to-all, but it cannot answer "here's the thing the Claude who just messaged me asked for." Reply-to-sender was named in ADR-0008 as the **common case** — targeted responses to the originating session, not broadcasts. Making it the uncommon case inverts the cost model and invites races when multiple Claudes are driving.

Forces at play:

- **Tokens are opaque bytes by design (ADR-0008).** The engine never synthesizes tokens; it only echoes ones the hub gave it. Any guest-facing API must preserve that property — the guest must not be able to fabricate a token.
- **The guest has no allocator.** Passing the raw token bytes in and back out works with guest linear memory, but lifetimes get awkward (the inbound mail dispatch doesn't own a buffer the guest can hold past the receive call).
- **A guest-local handle is the natural shape.** The substrate mints a small integer per inbound mail, hands it to the guest, and translates back to the real `SessionToken` on the send side. The guest stores `u32`s, not bytes.
- **Handle lifetime is bounded by the session.** ADR-0008 already says reply-to-sender mail can fail with `sessionGone`. A handle that outlives its underlying token fails the same way on use — no new failure mode.
- **The SDK surface is the real consumer.** ADR-0012 deferred reply-to-sender from the SDK on the grounds the host fn didn't exist. Landing it unblocks `Sink::reply_to(sender)` in `aether-component`.

## Decision

Add one host fn and one parameter to the receive shim. Introduce a guest-side `SenderHandle` (opaque `u32`) that the guest receives on inbound mail and can pass to a new `reply_mail` host fn to target the originating session.

### 1. Substrate-side sender table

The substrate maintains a per-instance table mapping `SenderHandle (u32) → SessionToken`. Entries are:

- **Allocated** when an inbound `HubToEngine::Mail` is dispatched to a component. A fresh handle is minted; the `SessionToken` is stashed.
- **Looked up** when the guest calls `reply_mail` with that handle.
- **Expired** on a "session gone" status from the hub (per ADR-0008), or on component replace/drop (per ADR-0010).

Handles are monotonically increasing `u32`s per component instance. Exhaustion at 2³² inbound mails-per-instance is not a real problem for V0; if it becomes one, the handle becomes a generational index.

Broadcast-origin mail (inbound mail where `sender` was `Broadcast`) has no meaningful reply target. For those, the handle is `SENDER_NONE` (`u32::MAX`). A `reply_mail` call with `SENDER_NONE` fails with a status.

### 2. Extended receive shim

The `receive` export grows a `sender` parameter:

```
fn receive(kind: u32, ptr: u32, count: u32, sender: u32) -> u32;
```

This is a breaking change to the guest ABI, but the only existing guest (`aether-hello-component`) is in this repo and doesn't read inbound mail beyond the empty-tick case. The SDK (ADR-0012) absorbs the change so component authors don't see it directly.

### 3. New `reply_mail` host fn

```
fn reply_mail(sender: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
```

Semantics:

- `sender` is a handle previously handed to the guest via `receive`.
- Resolves to a `SessionToken`; builds `EngineToHub::Mail { address: ClaudeAddress::Session(token), kind_name, payload, origin }` and routes it through the outbound path.
- Returns status codes: `0` success, `1` unknown handle, `2` session gone, `3` memory OOB, `4` kind not found. (Parallels the existing `send_mail` status surface.)
- `origin` is resolved the same way ADR-0011 resolves it for broadcast sends — from the calling mailbox's registered name.

The existing `send_mail` is **not** overloaded to accept sender handles. Addressing a `MailboxId` and addressing a session are conceptually different — the first stays on the substrate, the second crosses the hub boundary. Keeping two fns makes the capability distinction visible at the call site.

### 4. Guest-facing SDK shape (pointers forward into ADR-0012)

In `aether-component`:

- `Sender` — opaque wrapper around the `u32` handle. Not `Copy`; single-use semantics are *not* enforced, but cloning is deliberate.
- On inbound mail dispatch, the SDK surfaces a `&Sender` (or `Option<&Sender>` — `None` for broadcast-origin mail) alongside the typed payload.
- `ctx.reply(&sender, &payload)` wraps `reply_mail` with the same typed-send ergonomics as `ctx.send`.

A component that wants to remember a sender across receives can `sender.clone()` and stash it. The handle may expire before it's used again; the eventual reply attempt is the failure surface.

## Consequences

### Positive

- **Closes ADR-0008's stated common case.** Reply-to-sender is the routine interaction pattern; making it available to components turns request/response into a natural mail exchange.
- **Opaque to the guest.** The guest holds a `u32` handle, not a byte blob. Can't synthesize, can't introspect, can't forge.
- **Clean lifecycle story.** Replace/drop invalidates the table along with everything else (ADR-0010). Session disconnect invalidates individual handles with a specific status. No new "what happens when" edge case beyond what ADR-0008 already named.
- **Composes cleanly with ADR-0012.** The SDK's `Sink<K>` abstraction extends naturally to `Sender` — same typed-send body, different destination.

### Negative

- **Receive shim ABI change.** The fourth parameter breaks existing guests. Mitigated by the SDK landing simultaneously; the raw-FFI path for components that skip the SDK breaks but has one user today.
- **Per-instance sender table is a new substrate responsibility.** The component host already owns linker/ctx/registry; this adds a table. Small, but it's another thing to evict on replace/drop and to bound in size.
- **Handle exhaustion is theoretically possible.** 2³² inbound mails on a single instance is astronomical for V0; naming it is just hygiene.

### Neutral

- **Broadcast remains addressed by well-known name.** `"hub.claude.broadcast"` is still the broadcast target; reply uses the handle. Two different shapes for two different intents — the asymmetry is honest.
- **Origin attribution (ADR-0011) applies symmetrically.** Reply mail carries the calling mailbox's name as `origin`, same rule as broadcast.
- **Token format stays hub-internal.** Nothing about this ADR couples to how the hub mints tokens.

## Alternatives considered

- **Pass raw token bytes to the guest.** Guest reads the bytes into its own memory, passes them back verbatim. Rejected: lifetime and copy semantics are awkward; the handle indirection is strictly simpler and preserves the "opaque to guest" property better.
- **Overload `send_mail` with a discriminated recipient.** `send_mail(kind: RecipientKind, id: u32, ...)` where `RecipientKind` is `Mailbox | Session`. Rejected: one more branch on every send, and the capability distinction goes invisible. Two fns is cheaper and clearer.
- **Auto-expose the last sender as an implicit context.** `reply(kind, ptr, len, count)` that targets whatever triggered the current receive. Rejected: forecloses stashing senders for later replies, and ties reply semantics to receive scoping rather than to an explicit handle.
- **Signed tokens all the way to the guest.** Hub signs, guest holds signed blob, guest echoes it. Rejected: the per-instance handle indirection is cheaper and achieves the same unforgeability property at the trust boundary we actually have (substrate↔hub is trusted; guest↔substrate is the only place forgery could happen).

## Follow-up work

- Substrate: per-instance `SenderTable`, allocation on inbound dispatch, expiry hooks on replace/drop/session-gone.
- Wire-through on the receive shim ABI: fourth param `sender: u32`, `SENDER_NONE` sentinel for broadcast-origin.
- New `reply_mail` host fn in `aether-substrate/src/host_fns.rs` with full status codes.
- `Sender` type and `ctx.reply` in `aether-component` (blocked on ADR-0012 landing).
- Port the receive-side of `aether-hello-component` to read sender for a trivial round-trip smoke test (Claude sends `aether.ping`, component replies with `aether.pong`).
- **Parked, not committed:** generational sender handles, multi-reply semantics (one sender, N replies over time — already works; naming it if constraints emerge), cross-instance sender migration on replace (tied to ADR-0016).
