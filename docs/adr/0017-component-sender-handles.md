# ADR-0017: Extend `Sender` handles to component-origin mail

- **Status:** Proposed
- **Date:** 2026-04-17

## Context

ADR-0013 landed reply-to-sender for Claude-originated mail. A component receiving a session-addressed mail gets an opaque `Sender` handle on the receive shim's 4th param and can call `ctx.reply(sender, kind, &payload)` to answer. For component-originated mail and broadcast-origin mail, the handle is `SENDER_NONE` — deliberately, because `reply_mail` routes through `HubOutbound` and only understands session addressing.

That cut looked clean at the time. It isn't holding up. Concretely, there's no ergonomic way today for one component to answer another component that it didn't know about at init:

- **Known-at-init peers** work via `Sink<K>` — component B resolves a sink against component A's registered name and sends replies there. Fine when A and B know each other's names.
- **Runtime-discovered peers** have no clean path. A component that receives `demo.request` from some caller it's never heard of has to:
  - Carry the caller's registered name in the payload (bytes overhead; doesn't fit the Pod kind model cleanly for variable-length strings).
  - Call `resolve_kind` + `resolve_mailbox` from `receive` on every inbound (two host-fn round trips per request; bypasses the `InitCtx`/`Ctx` "resolve at init" discipline the SDK was built around).
  - Rebuild a `Sink<Reply>` for each message or cache a `HashMap<String, Sink<Reply>>` keyed by caller name.

None of that is unworkable — but all of it is friction the session-origin case avoids by threading an opaque handle through. The asymmetry is the problem: the *common-case* reply pattern works for Claude sessions and doesn't work for components.

Forces at play:

- **Runtime component loading (ADR-0010) is how components arrive.** A component loaded at T=1 can't have been resolved against component at T=0's init. Any request/response between them has to discover identity at receive time.
- **The `SenderHandle` plumbing already exists.** The per-instance `SenderTable` on `SubstrateCtx`, the 4th param on the receive shim, the `reply_mail` host fn — the shape is there; extending it is cheaper than inventing a parallel mechanism.
- **Two sender kinds have different lifetime rules.** Session handles expire on session-gone (hub-visible, async). Component handles expire on `drop_component`/`replace_component` (substrate-internal, synchronous). Unifying them means the substrate has to track both.
- **Typed sinks are the idiomatic outbound path.** `Sink<K>` fixes the kind at resolution; reply by handle fixes only the addressing. This ADR doesn't change `Sink<K>` — it adds a second outbound path for runtime-discovered peers.

## Decision

Widen `SenderTable` entries to cover component origins. `Component::deliver` allocates a handle for component-origin mail too; `reply_mail` routes to either `HubOutbound` or the local `MailQueue` based on the handle's variant.

### 1. Table entry shape

Replace the existing `HashMap<u32, SessionToken>` with:

```rust
enum SenderEntry {
    Session(SessionToken),
    Component(MailboxId),
}
```

`SenderTable::allocate` takes a `SenderEntry`; `resolve` returns `Option<SenderEntry>`. The guest-visible surface (opaque `u32` handle) is unchanged.

### 2. `Component::deliver` allocates for both origins

Today `deliver` allocates a handle only when `Mail.sender != SessionToken::NIL`. Widen:

```rust
let handle = match (mail.sender, mail.from_component) {
    (tok, _) if tok != SessionToken::NIL => alloc(SenderEntry::Session(tok)),
    (_, Some(mbox)) => alloc(SenderEntry::Component(mbox)),
    _ => SENDER_NONE,
};
```

This implies `Mail` grows a `from_component: Option<MailboxId>` field. The existing `ctx.send` path in `SubstrateCtx::send` already has `self.sender` — it's the sender's own mailbox id — so populating this is a one-line change: `Mail::new(...)` learns `.with_origin(id)`, `ctx.send` calls it.

Broadcast-origin (hub broadcast or sink-handler-originated) still produces `SENDER_NONE`.

### 3. `reply_mail` branches on variant

```rust
match ctx.sender_table.resolve(sender) {
    Some(SenderEntry::Session(token)) => {
        ctx.outbound.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(token),
            kind_name, payload, origin,
        }));
    }
    Some(SenderEntry::Component(mbox)) => {
        ctx.send(mbox, kind, payload, count);
    }
    None => return REPLY_UNKNOWN_HANDLE,
}
```

The existing `SubstrateCtx::send` path already handles "mailbox is Dropped" — mail to a dropped mailbox is discarded with a log. Reply to a component that's been dropped after the request was delivered silently drops, same as any other send to a dropped mailbox. No new status code needed.

### 4. Handle lifetime

- **Session handles**: same as ADR-0013. Valid for the life of the session; stale on session-gone (V0 doesn't detect this synchronously — the hub discards the frame).
- **Component handles**: valid for the life of the *receiving* component instance. Drop/replace on the receiver clears the table along with the `Store`. The *referenced* component being dropped after handle allocation resolves at `reply_mail` time — it routes to a Dropped mailbox entry and the substrate discards, consistent with today's component-to-dropped-mailbox semantics.

Crucially, **handles do not cross instance boundaries**. A handle allocated for component B, stashed by B, then carried through a `replace_component` on B is invalidated along with the rest of B's state (ADR-0010). If B wants to survive the reply across replace, the payload needs to carry something the new instance can re-resolve — the handle isn't that thing.

### 5. SDK surface

No change to `Sender` or `Ctx::reply`. The guest holds a `u32`, passes it back; the substrate does the routing. From the component author's perspective:

```rust
fn receive(&mut self, ctx: &mut Ctx, mail: Mail) {
    if let Some(req) = mail.decode(self.request)
        && let Some(sender) = mail.sender()
    {
        ctx.reply(sender, self.response, &Response { ... });
    }
}
```

Works identically whether the caller was a Claude session or another component. The component author writes one code path.

### 6. What this ADR does not do

- **No new SDK types.** `Sender` stays opaque; no `ComponentSender` variant. The failure modes are indistinguishable from the guest's perspective — "reply failed" covers both session-gone and component-dropped. If that ever needs to be distinguishable, it's an additive follow-up.
- **No named handles or handle introspection.** The guest can't ask "is this a session or a component?" The substrate knows; the guest doesn't need to.
- **No cross-instance handle migration.** Handles invalidate on replace/drop, same as today.
- **No `origin_mailbox()` accessor on `Mail`.** Deferred — the handle-based reply covers the motivating use case; exposing the raw `MailboxId` is orthogonal and can come later if a real need surfaces (e.g., logging, auditing).
- **No change to `ctx.send(Sink<K>, ...)`.** Typed sinks remain the right choice when the peer *is* known at init.

## Consequences

### Positive

- **Symmetric reply API.** One `mail.sender()` / `ctx.reply(...)` path regardless of whether the caller is a Claude session or another component. Component authors don't learn two patterns.
- **Runtime-discovered peer replies become ergonomic.** No more "embed your own name in the payload" workaround; no more per-message `resolve_mailbox` calls.
- **Cheap implementation.** Extends the existing `SenderTable` — roughly one enum, one `Mail` field, and a match in `reply_mail`. No new host fn, no new guest type.
- **Composes with typed sinks.** Components that know their callee at init still use `Sink<K>`; components that answer discovered callers use `Sender`. Both work.

### Negative

- **Two handle lifetimes under one type.** Session handles and component handles expire under different rules. The opaque-u32 surface hides this from the guest, but substrate code has to reason about both cases. A component dropped between receive and reply is a new failure mode from today's perspective (though the same mail-to-dropped-mailbox semantics apply).
- **Forgery surface is slightly wider.** A compromised component that guesses a handle value could reply to an arbitrary session or component. Same concern as ADR-0013, just scoped wider — still bounded by the guest having to invent valid `u32`s, which is astronomical against monotonic allocation.
- **Encourages reply-over-send.** For peer-to-peer patterns that *could* use `Sink<K>` cleanly, developers might default to reply because the API is uniform. That's mostly fine but skips the compile-time kind fencing sinks provide.

### Neutral

- **Wire format unchanged.** The 4th param on receive is already `u32`; this just widens the set of values that can be non-`SENDER_NONE`.
- **No hub-protocol changes.** `EngineToHub::Mail` and `HubOutbound` stay put; the component-reply path routes entirely through the substrate's local `MailQueue`.
- **`origin` attribution (ADR-0011)** is symmetric — a component reply carries the replying mailbox's name as `origin`, same rule as today's broadcast sends.

## Alternatives considered

- **Separate `ComponentSender` type.** Give the guest two distinct opaque types so reply semantics are visible at the call site. Rejected: doubles the API surface for a distinction the guest doesn't care about and the substrate can already carry in its own table. Also forces the macro to emit two shims or a tagged parameter, complicating the ABI.
- **Named reply via payload convention.** Keep today's workaround and document it — bake in "carry your name in a reply_to field" as the idiom. Rejected: strings in Pod kinds are awkward, the per-message resolve cost is real, and the ergonomics are measurably worse than the handle approach for the use cases that matter (request/response between runtime-loaded components).
- **`origin_mailbox()` accessor only.** Expose the caller's `MailboxId` and let the guest build a `Sink<K>` manually. Rejected for this ADR: doesn't solve the kind-discovery problem (guest still needs the kind id for the reply) and the handle path is strictly cheaper on the wire.
- **Defer until a second component needs it.** Previously leaned toward this. Rejected: the runtime-discovery case is the real shape the engine is built for — ADR-0010 exists precisely so components can arrive without prior knowledge — and forcing every future component author to invent a workaround around a single missing host-fn case is the kind of friction that compounds.

## Follow-up work

- Substrate: `SenderEntry` enum on `SenderTable`, `Mail` growing `from_component: Option<MailboxId>`, `ctx.send` populating it, `reply_mail` branching on variant.
- Tests: deliver with component origin allocates a handle; reply over that handle enqueues on `MailQueue` (not `HubOutbound`); reply to dropped component silently discards; `SENDER_NONE` from broadcast-origin still returns `REPLY_UNKNOWN_HANDLE`.
- A second component crate (or a second hello variant) that actually exercises request/response — without it, we're trusting the mechanism without a consumer.
- **Parked, not committed:** distinguishing session-gone from component-dropped at the guest surface, handle migration across `replace_component`, `Mail::origin_mailbox()` accessor, forgery-resistance via capability-unguessable handles.
