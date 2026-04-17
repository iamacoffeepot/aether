# ADR-0018: Constrain `Sender` to the receive call

- **Status:** Proposed
- **Date:** 2026-04-17

## Context

ADR-0013 shipped `Sender` as a plain `Copy` handle that the guest can stash wherever it likes — in `self`, in a collection, across receives. That matches the wire contract (the underlying `u32` is valid for the life of the substrate's sender-table entry) but it leaves the guest holding a capability whose expiry rules it doesn't see.

The failure modes that fall out of persistable senders:

- **Session-gone.** The originating Claude session disconnects; the `u32` is still valid substrate-side but the hub discards the frame on receipt. The guest gets no signal.
- **Component-dropped** (if ADR-0017 lands). The referenced component is dropped between the original receive and the later reply; the substrate discards.
- **Cross-replace invalidation.** The instance that holds the stashed sender is itself replaced via `replace_component`; the new instance has a fresh (empty) sender table and any `u32` smuggled through state migration is meaningless.

None of these are bugs against the current contract — mail is best-effort, and the drop-on-stale behavior is consistent with every other send path. But all three are silent: the guest pays attention to "did my reply get there?" with no feedback, and the right answer for every one of them is the same ("build acks on top"). If the common case is synchronous request/response — reply in the same receive that delivered the mail — the stale-handle surface exists only to support a rarer pattern nobody has built yet.

The architectural frame from the ADR-0017 review:

- **`Sink<K>`** is the declared-dependency shape. Resolved at init against a known peer. Strong coupling; compile-time kind check; long-lived.
- **`Sender`** is the ephemeral-answer shape. Arrives with the mail; used to respond to *this* delivery. Weak coupling; opaque identity; short-lived.

Persistable senders blur that split. A stashed sender is effectively a runtime-resolved `Sink` with no compile-time kind check and looser lifetime rules. If the intent is "persistent outbound to a known peer," `Sink<K>` already does that better. If the intent is "answer this caller right now," nothing needs persistence.

Forces at play:

- **Rust can enforce this at compile time.** A lifetime parameter on `Sender` tied to the inbound `Mail<'_>` makes stashing in `&mut self` a type error, not a convention.
- **The synchronous case is the one we've actually built.** Hello's ping→pong replies in the same receive. The ADR-0017 motivating case (runtime-discovered peer reply) is also synchronous.
- **Asynchronous completion has a known alternative.** A component that needs to answer later can take the re-resolve cost: caller includes a name, callee resolves a `Sink<Reply>` when the work finishes. Strictly more work than stashing a sender, but it keeps the handle-persistence concern out of the system.
- **The substrate's sender table can get simpler.** If no handle outlives the receive call, the table's scope collapses to "populate on entry, clear on exit" — bounded memory, no per-instance growth to reason about.

## Decision

Bind `Sender` to the inbound mail's lifetime. A component may use a sender only during the receive call that produced it; the compiler rejects any attempt to stash it.

### 1. Type change

```rust
pub struct Sender<'a> {
    raw: u32,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> Mail<'a> {
    pub fn sender(&self) -> Option<Sender<'a>> { ... }
}
```

The existing `Copy` impl stays — a component that wants to reply twice from one receive (e.g. partial + final for a streaming answer) just copies the handle. No `Clone` wouldn't add anything; the binding is already the restriction.

`Ctx::reply` takes `Sender<'_>` by value:

```rust
impl<'c> Ctx<'c> {
    pub fn reply<K: Kind + bytemuck::NoUninit>(
        &self,
        sender: Sender<'_>,
        kind: KindId<K>,
        payload: &K,
    ) { ... }
}
```

No lifetime relationship is required between `Ctx<'c>` and `Sender<'_>` — the `Sender<'a>` is independently tied to the `Mail<'a>`, which lives for the receive call, which is where `Ctx<'c>` is also scoped. Stashing in `&mut self` fails because `self: &mut Self` is `'static`-ish (or at least outlives any single `'a`) and the struct field would require `Sender: 'static`, which it isn't.

### 2. Substrate table lifecycle

With persistence removed, the per-instance `SenderTable` no longer needs to grow across receives. `Component::deliver` can:

1. Allocate the entry (if non-NIL sender).
2. Call `receive(...)`.
3. Clear the entry on return.

The substrate may choose to keep the table around (one entry per alive receive call, which is always 1 in the single-threaded dispatch model) or flatten to a single slot: `current_sender: Option<SenderEntry>` on `SubstrateCtx`. The latter drops to `Option` and a u32 branch; no allocation, no HashMap. Implementation choice, not a contract.

Either way, a guest that stashes the `u32` as a plain integer and tries to use it next receive gets `REPLY_UNKNOWN_HANDLE` — which is the failure the type system was trying to prevent in the first place, now caught at runtime as a belt-and-suspenders.

### 3. Async completion is out of scope

A component that receives a request, computes across multiple ticks, and replies later cannot use `Sender` for the reply under this ADR. The idiomatic path:

- Caller includes its own registered name in the payload (Pod-friendly: `[u8; N]` or an `Opaque` kind that postcard-encodes a string).
- Callee resolves a `Sink<Reply>` to that name when the work finishes (caching in a `HashMap` if the same name is common).

This is strictly more work than stashing a sender. It's also the honest cost of asynchronous reply in a best-effort mail system. If a future use case makes this intolerable, that's the moment for a dedicated *deferred-reply handle* — distinct from `Sender`, with its own expiry semantics and explicit survival contract. Today, we don't have that use case.

### 4. What this ADR does not do

- **No change to the wire / host-fn shape.** `reply_mail` still takes `(sender, kind, ptr, len, count) -> u32`. The enforcement is purely in the guest SDK.
- **No new host fn.** `save_sender_for_later` or similar is not introduced. Deferred reply is explicitly parked.
- **No change to broadcast or sink paths.** `ctx.send(Sink<K>, ...)` is unaffected.
- **No renaming.** `Sender` keeps its name; only the lifetime parameter appears.

## Consequences

### Positive

- **Stale-handle failures become a compile error.** The session-gone, component-dropped, and cross-replace concerns from ADR-0017's review collapse — the sender can't outlive its context.
- **Sharpens the `Sink<K>` vs `Sender` split.** `Sink<K>` = declared at init, long-lived, type-checked peer binding. `Sender<'a>` = ephemeral, opaque, scoped to one receive. Two clearly different shapes; no overlap.
- **Simpler substrate lifecycle.** Sender table has bounded scope, optionally collapses to `Option<SenderEntry>`. No "clean up on drop/replace" concern because the entry is already gone by receive return.
- **No SDK shape churn if we ever widen.** If a deferred-reply mechanism does land later, `Sender` stays ephemeral and the new thing gets its own type and name. Fewer retrofits.

### Negative

- **Closes the async-completion shape.** Any component that wants to reply across multiple receives has to take the re-resolve cost. This is real friction for patterns that don't exist yet but might.
- **Compile errors instead of runtime errors for stash attempts.** This is listed as negative because the error message from "lifetime `'a` doesn't live long enough" isn't as clear as "you cannot stash senders." Mitigated by documentation on `Sender` that explicitly names the constraint.
- **Needs a type change to an already-accepted API.** ADR-0013 shipped `Sender` without a lifetime. This ADR modifies that shape. Net cost is small (hello-component doesn't stash; no other guest exists) but it is a breaking change to anyone who wrote against the 0013 surface in the week between PRs.

### Neutral

- **Best-effort contract preserved.** Reply still might not arrive (hub dropped, component dropped mid-receive, kind unknown). Lifetime scoping reduces the *duration* during which staleness can happen, but the fundamental "might not get there" nature of mail is unchanged.
- **Opaque-u32 surface preserved.** The guest still sees a handle with no structure. Lifetime is a Rust-side fence; the underlying protocol is unchanged.

## Alternatives considered

- **Leave `Sender` persistable (status quo).** Rejected because the common case is synchronous reply; persistence costs real runtime complexity (sender table grows per instance, entries need lifetime rules) to support a use case nobody has built.
- **Separate types for sync and async reply.** `Sender<'a>` for synchronous, `DeferredSender` (owned, persistable) for async. Rejected for V0: we don't have a concrete async use case to design the second type against, and introducing two names now when everyone uses one is speculative complexity. Revisit when async shows up.
- **Runtime-only enforcement.** Invalidate the sender at the end of every receive; a guest that stashes and reuses gets `REPLY_UNKNOWN_HANDLE` at runtime. Rejected: the type system can enforce this for free, and a compile error beats a silent drop.
- **Document "don't stash" as a convention.** Rejected: conventions erode. This one isn't load-bearing enough to carry across contributors without a compile-time check.
- **`Sender` takes a lifetime tied to `Ctx<'_>`** instead of `Mail<'_>`. Rejected: `Ctx<'_>` lives for the full receive call; `Mail<'_>` is the inbound that *produced* the sender. Tying to `Mail` is semantically correct and also lets the substrate scope entries tightly (they don't need to outlive the mail).

## Follow-up work

- SDK: widen `Sender` to `Sender<'a>`, update `Mail::sender()` signature, update `Ctx::reply` signature. Doc-comment the lifetime discipline.
- Substrate: optionally simplify `SenderTable` to a single-slot `current_sender: Option<SenderEntry>` on `SubstrateCtx`. Not required by this ADR — the HashMap form is still correct with bounded-by-one entries — but a reasonable cleanup.
- Tests: guarded test that fails to compile if a component stashes a `Sender`. (e.g. a `trybuild` test or a commented doc example.)
- Update `hello-component` ping handler: current code doesn't stash, so no changes expected beyond the signature refresh.
- **Parked, not committed:** deferred-reply handle for async completion (if the use case ever materializes), per-session-gone detection, distinguishing reply failure modes at the guest surface.
