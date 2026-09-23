# ADR-0232: Flat Ctx Send Verbs

- **Status:** Accepted
- **Date:** 2026-09-23
- **Amended:** 2026-09-23 — §3's flat subscribe names its publisher: `ctx.subscribe::<LifecycleCapability, Tick>()`, both type parameters caller-chosen and checked by the existing `Publishes<K>` marker, because the `PublishedBy` link cannot be implemented under Rust's orphan rule. Window events subscribe every window by default.
- **Amended:** 2026-09-23 — typed flat sends take the payload as `&impl SendableTo<R>`, so the turbofish names only `R`; the native ctx's erased `send_with_context` is renamed, and the held-reference context verbs are `send_to_with_context` / `send_detached_to_with_context`; the ctx for `unwire` and `on_rehydrate` is typed by `Self` too; the per-cap handle facades and `MailboxForward` are deleted.

Amends [ADR-0230](0230-proven-actor-references.md) §5 (what replaces the
deleted `ctx.actor::<R>()` handle at the call site),
[ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
(its flat `ctx.send::<R>(&k)` shape is adopted), and
[ADR-0231](0231-protocol-typed-references-and-reply-checks.md) §3 and §7 (the
send surface its sketches spell as `ctx.to(&r).send`).

## Context

ADR-0230 §5 deletes the unproven handle: `ctx.actor::<R>()` folds
`R::NAMESPACE` with the caller's scope into a position on every call and hands
back a `WasmActorMailbox` / `NativeActorMailbox` that sends with no liveness
check. §5 says what goes; it does not say what a call site writes instead.
Issue #6355 records the owner's direction: the proof should come from
`#[actor(depends(R))]`, and the call site should be a flat verb.

The pieces already exist:

- `#[actor(depends(R))]` emits `impl DependsOn<R> for Self` and a dependency
  record the host checks before `init` (ADR-0230 §3; the refusal lives in the
  component host's `dependencies.rs` and the native `dependencies.rs`).
- `ctx.actor_ref::<R>()` already mints an `ActorRef<R>` bounded on
  `A: DependsOn<R>`, with no host call.
- `ctx.actor::<R>()` is bounded on `A: Reaches<R>`
  (`crates/aether-actor/src/model/mod.rs`), which the erased ctx satisfies for
  every `R`. Handlers get the erased ctx today, so the handle reaches actors
  nobody declared.
- ADR-0231 §7 types the handler ctx by its actor (`WasmCtx<'_, Self>`,
  `NativeCtx<'_, Self>`, `WireCtx<'_, '_, Self>`). Once it lands, a verb bounded
  on `Self: DependsOn<R>` checks the declaration at every call site.
- ADR-0075 proposed `ctx.send::<RenderCapability>(&triangle)` before the handle
  existed. It never shipped in that form.

The migration is uneven. At `e4e5114bd`, the tree has 133 non-comment lines
that call `.actor::<` (117 outside test directories), and 17 `depends(`
entries on `#[actor]` attributes, of which 4 are outside tests and fixtures
(the fleet proxy's two, and one each on the puppet idle and turntable actors).
Both counts come from these commands, run at the repository root:

```sh
git grep -nE '\.actor::<' -- 'crates/*.rs' | grep -vE ':[0-9]+:\s*//' | wc -l
git grep -hE '^\s*#\[(aether_actor::)?actor\(.*depends\(' -- 'crates/*.rs' \
  | grep -oE 'depends\(' | wc -l
```

## Decision

### 1. Flat verbs replace the handle

Every `ctx.actor::<R>().verb()` chain becomes a verb on the ctx. The target is
the turbofish, and a turbofish names only types the caller chooses, never `_`.

```rust
ctx.send::<RenderCapability>(&frame);              // chained to the handler's causal chain
ctx.send_detached::<RenderCapability>(&frame);     // fresh chain
let id: MailId = ctx.send_tracked::<Puppet>(&load); // for settlement subscription
ctx.send_many::<RenderCapability>(&triangles);
ctx.send_ignoring_reply::<Puppet>(&load);          // ADR-0231 §1's written opt-out
ctx.send_to(&mesh_loader, &LoadMesh { path });     // an ActorRef<R> or ProtocolRef<P>
let render: ActorRef<RenderCapability> = ctx.actor_ref::<RenderCapability>();
```

`ctx.send::<R>` carries ADR-0231 §1's reply bound with `A = Self`: if `R`
answers `K` with `O`, the sender must handle `O`. `send_detached`,
`send_tracked`, and `send_many` carry the same bound. `send_detached` keeps its
name. `ctx.send_to` takes a held reference through the `Target<T>` trait of
ADR-0231 §3; it is flat, and there is no `ctx.to(&r)` handle. An
`ErasedActorRef` passed to `send_to` keeps ADR-0231 §4's narrowing to
publishing. `ctx.actor_ref::<R>()` keeps its name and mints a proof for a
declared dependency, for code that stores the reference.

*(Amended 2026-09-23: the context-carrying sends follow the same split. The
typed verb is `ctx.send_with_context::<R>(&k, &c)`, and a held reference sends
with a context through `ctx.send_to_with_context(&r, &k, &c)` and
`ctx.send_detached_to_with_context(&r, &k, &c)`, matching `send_to`. The native
ctx's inherent erased `send_with_context(&ErasedActorRef, &K, &C)` and its
detached twin are renamed into that family, which frees the name for the typed
verb.)*

### 2. The proof is the declaration

```rust
#[actor(depends(RenderCapability), depends(LifecycleCapability))]
impl WasmActor for Camera {
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Self>, _tick: Tick) {
        ctx.send::<RenderCapability>(&ViewProjection { view_proj: self.view_proj() });
    }
}

impl<A, M> WasmCtx<'_, A, M> {
    pub fn send<R, K: Kind>(&mut self, payload: &K)
    where
        A: DependsOn<R>,
        R: Contract<K>,
        <R as Contract<K>>::Reply: ReplyHandledBy<A>,
    { /* the existing send path */ }
}
```

*(Amended 2026-09-23: this sketch cannot be called as §1 writes it. Rust has
no partial turbofish, so `send<R, K>` would be spelled `ctx.send::<R, _>`, and
a turbofish never carries `_` (§1). Every typed flat send takes its payload as
`payload: &impl SendableTo<R>`, a blanket over `K: Kind` where `R` handles `K`,
so the turbofish names only `R`. The bounds are unchanged: `A: DependsOn<R>`
and ADR-0231 §1's reply bound.)*

A declared dependency is checked `Live` before `init`, so a flat send to it is
infallible at the call site: no `Option`, no `Result`. The handler ctx is typed
by its actor (ADR-0231 §7), so `ctx.send::<R>` compiles only when
`Self: DependsOn<R>`. An undeclared target is a compile error at the send, not
a warn-drop at run time. The erased ctx has no `DependsOn` impls, so it has no
flat send to a named actor.

### 3. Flat subscribe

```rust
pub trait PublishedBy: Kind {
    type Publisher: Publishes<Self>;
}

ctx.subscribe::<Tick>();                           // publisher: LifecycleCapability
ctx.subscribe::<Key>(WindowSelector::All);         // publisher: WindowCapability
```

Each event kind names exactly one publisher through its `PublishedBy` link. The
trait has one associated type, so a second publisher cannot be named, and
`ctx.subscribe::<K>` infers the target from `K`. A filter stays an argument.
The call carries ADR-0231 §8's bound (the subscriber's row for `K` is silent)
and requires `Self: DependsOn<K::Publisher>`, the same proof as a send.

**Amendment (2026-09-23): the verb names the publisher.** The form above cannot
be written. `impl PublishedBy for Tick { type Publisher = LifecycleCapability; }`
names a kind and a publisher that live in different crates: the event kinds are
in `aether-kinds`, which `aether-actor` depends on, and the publishers are in
cap crates above `aether-actor`. In every crate that can see both, the impl is
a foreign trait on a foreign type, which Rust's orphan rule refuses, and moving
either side creates a dependency cycle. The two call lines above also ask one
method to take no argument for one kind and a selector for another, which a
single Rust method cannot do.

```rust
ctx.subscribe::<LifecycleCapability, Tick>();
ctx.subscribe::<WindowCapability, Key>();
```

Both type parameters are chosen by the caller, so the turbofish rule (§1)
holds. The pair is checked by the existing `P: Publishes<K>` marker, together
with ADR-0231 §8's silent-handler bound and `Self: DependsOn<P>`, so a
publisher that does not publish `K`, a subscriber that answers `K`, and an
undeclared publisher are each a compile error at the call. Window events
subscribe every window: each window subscribe in the tree passes
`WindowSelector::All`, so the verb takes no selector. A per-window filter, if
one is ever needed, is its own window verb. The `PublishedBy` trait is not
added.

### 4. A dependency that dies after load

A send to a declared dependency that has since died drops quietly. Mail is best
effort (ADR-0231, Context), and the dependency check proves only that the peer
reached `Live`, which stays true (ADR-0230 §1). A caller that cares about the
peer's death monitors it: `ctx.monitor` with the reference from
`ctx.actor_ref::<R>()` (ADR-0079 §8). Declaring a dependency implies no
monitoring.

### 5. Native capabilities use the same model

A native capability that mails another declares the dependency through the
chassis composer at boot. The declaration is the same `#[actor(depends(R))]`
record, which the builder already checks `Live` before `init` at every native
birth site (ADR-0230 §3), and the composer hands the capability its proof. The
native flat verbs therefore carry the same `DependsOn<R>` bound and the same
infallibility as the wasm ones. *(Amended 2026-09-23: a dependency on a pumped
slot, such as render on desktop and the harness chassis, is checked against
the slot's Claim-stage reservation, and the boot fails if the pump never goes
`Live`; see ADR-0230 §3.)*

### 6. No optional peers

A capability that may be absent on a chassis is composed there as a stub that
claims its mailbox and answers honestly: it absorbs fire-and-forget mail, or
replies `Err` to a request. `HeadlessRenderCapability`,
`HeadlessWindowCapability`, `HeadlessAudioCapability`,
`HeadlessClipboardCapability`, and `UnsupportedSubstrateHarnessCapability` are
the existing instances. A declared dependency therefore always holds, on every
chassis. There is no `optional(R)` attribute and no `send_optional` verb.

### 7. What is deleted

The `ctx.actor::<R>()` handle on every ctx, every `ctx.actor::<X>().verb()`
chain, and the `Reaches<R>` bound that let the erased ctx reach any actor.
Facade traits that hang verbs off the handle (`LifecycleMailboxExt`, the window
facade's `subscribe`) move to flat ctx verbs.

*(Amended 2026-09-23: the per-cap facades do not move; they are deleted, along
with `MailboxForward`, the trait they forward through. That covers
`FsMailboxExt`, `ClipboardMailboxExt`, `HttpMailboxExt`,
`WindowManagerMailboxExt`, `WindowMailboxExt`, and `LifecycleMailboxExt`. Their
bodies are one-line kind literals, so a call site sends the kind directly
through a flat verb:
`ctx.send::<FsCapability>(&Read { addr: NamespaceAddr::new(ns, path) })`. The
only flat verb that replaces a facade method is §3's subscribe and its
unsubscribe twin. Keeping the facades as extension traits on the ctx would
leave a second way to send per cap.)*

## Consequences

- An undeclared target is a compile error at the call site, and a declared one
  is checked at load, so the dependency list is the complete statement of who
  an actor mails by type.
- Call sites get shorter and drop the handle's lifetime: one verb, one type,
  one payload.
- Migration adds `depends(R)` declarations before it removes a chain: at the
  base there are 17 declarations against 133 call lines, so most actors gain a
  dependency list. The work follows ADR-0231 §7's ctx typing and the
  mechanical ADR-0230 contract bundles, one crate per change, because it
  touches nearly every crate. No change adds `MailboxId` surface.
- A load now fails where a send used to warn-drop, on any chassis missing a
  dependency. The stub rule (§6) makes that a composition error to fix once,
  not a run-time branch in every caller.
- Every event kind gains a `PublishedBy` impl beside its `#[kind]` declaration.
  *(Amended 2026-09-23: no `PublishedBy` impl is added; the publisher is named
  at the subscribe call and checked by the existing `Publishes<K>` impls, §3.)*
- The names above close the naming pass issue #6355 asks for.

## Alternatives considered

- **`ctx.to(&r).send(&k)` for held references.** A second handle, reintroduced
  for one case. Rejected: `ctx.send_to(&r, &k)` is flat and says the same.
- **Keep the handle, bounded on `DependsOn`.** Proven, but it keeps the
  two-step chain and a borrowed handle type on every call site. Rejected on
  readability, the owner's reason for the flat form.
- **Optional peers with `send_optional`**, an honest `Option` lookup for a
  peer that may be absent. Rejected: every caller gains a branch the chassis
  should answer once, and a stub that claims the mailbox already exists for
  every capability that is absent somewhere.
- **Implied monitoring on dependency death.** A registry watch per declared
  dependency, paid by every actor whether it cares or not, and a policy (drop,
  refuse, restart) the framework would have to pick for everyone. Rejected:
  `ctx.monitor` is the opt-in that exists.
- **A multi-publisher subscribe** (`ctx.subscribe::<R, K>()`). Rejected: no
  event kind has two publishers, so the second parameter only restates the
  first, and a `PublishedBy` link keeps it that way by construction.
  *(Amended 2026-09-23: adopted. The `PublishedBy` link cannot be implemented
  under the orphan rule (§3), so the publisher parameter is the only place the
  publisher can be named. It is checked by `Publishes<K>` rather than restated
  unchecked.)*
- **One subscribe verb per publisher** (`ctx.subscribe_lifecycle::<Tick>()`,
  `ctx.subscribe_window::<Key>()`), each added to the ctx by its cap's crate.
  Considered on 2026-09-23 when §3's first form proved unwritable. Not adopted:
  `ctx.subscribe::<P, K>()` declares both types in one verb, and per-cap verbs
  would add a name per publisher.
- **Moving the event kinds and their publishers into one crate above
  `aether-actor`**, which would let the `PublishedBy` impls compile. Rejected:
  a large restructure, and it conflicts with ADR-0122's rule that a cap's
  identity lives in its own crate.

## Amendments

- **ADR-0230 §5.** The deletion of `ctx.actor::<R>()` stands. Its replacement
  at the call site is the flat verbs above, proven by `depends(R)`; the
  `Reaches<R>` bound goes with it.
- **ADR-0075.** Its decision 2 shape, `ctx.send::<R>(&k)`, is adopted with
  `Self: DependsOn<R>` in place of `R: Singleton`, and its
  `resolve_actor::<R>(name)` stays deleted (ADR-0230 §5).
- **ADR-0231 §3 and §7.** Their sketches' `ctx.to(r).send(&k)` reads
  `ctx.send_to(r, &k)`. §7's ctx typing is what makes the `DependsOn` bound
  checkable, and it supersedes issue #6355's note that no actor-typed ctx
  migration is needed.
- **ADR-0231 §7 (2026-09-23).** The ctx typing extends past handlers and
  `wire`: `unwire` and `on_rehydrate` also receive a ctx typed by `Self`, so a
  send from either hook carries the same `DependsOn` bound. `on_rehydrate`'s
  fixed `WasmCtx<'_>` parameter becomes `WasmCtx<'_, Self>`.
