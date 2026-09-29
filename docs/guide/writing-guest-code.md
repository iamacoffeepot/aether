# Writing guest code

To run your own code on a running engine, write a **component**: a full actor,
with its own vocabulary, mailbox, and subscriptions, that you compile to wasm.
The one authoring surface deploys in two shapes.

## The two deployment shapes

| Mechanism | When it arrives | Where it runs |
|---|---|---|
| `#[actor]` + `export!` inline children | compile time | inside the cluster |
| `load_component` | runtime | its own instance & lineage |

A component authored with `#[actor]` gives you an actor either way you deploy it:
compiled inline as a child of another actor, it settles mail cascades inside the
cluster; loaded on its own with `load_component`, it becomes an independent
instance with its own lineage. Both are the same authoring surface — the actor
you write, [compiled to wasm](recipes/writing-a-component.md).

An inline child lives until it is despawned or its parent closes. Either one
closes the child and spends its name
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md) §8),
so spawning a despawned child's key again fails with
`SpawnError::AliasAllocationFailed`; spawn a fresh child under a new key instead
([the actor model](foundations/actor-model.md) has the details).

## Deferred replies

A `#[handler::single]` that answers later returns `Pending<R>`, so its row still
declares the reply kind `R`
([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)).
`ctx.hold::<R>()` returns the pair: the handler returns the `Pending<R>` receipt,
and keeps the `Held<R>` ticket to answer from any later handler with
`held.answer(ctx, &reply)`. The host keeps the request's reply handle and holds
its settlement open until the ticket answers.

```rust
#[handler::single]
fn on_load(&mut self, ctx: &mut WasmCtx<'_>, msg: LoadMesh) -> Pending<MeshLoadResult> {
    let (pending, held) = ctx.hold::<MeshLoadResult>();
    let read = Read { addr: NamespaceAddr::new(&msg.namespace, &msg.path) };
    let _ = ctx.send_with_context::<FsCapability>(&read, MeshLoadContext { held, path: msg.path });
    pending
}

#[handler::single]
fn on_read(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult) {
    let Some(context) = ctx.take_context::<MeshLoadContext>() else { return };
    context.held.answer(ctx, &MeshLoadResult::from(result));
}
```

A `Held<R>` is move-only. Park it in actor state, in a request context passed by
value to `send_with_context`, or in the state `on_dehydrate` saves with
`save_state_kind`; a take or `PriorState::decode_kind` claims it back. Misuse
fails fast:

- a second `hold` in one dispatch panics, since one request owes one reply;
- a `Pending` dropped instead of returned panics;
- a `Held` dropped unanswered panics, unless it was parked in a context or saved
  state, or its mail had no reply target;
- a reply whose stored context holds a `Held` must take that context, or the
  guest panics after the handler returns, naming the context kind;
- `on_dehydrate` refuses the replace while a `Held` is still live and unsaved,
  and the host keeps the old instance running.

`R` must implement `aether_actor::HeldReply`, whose `unanswered()` names the
failure reply the caller receives if the guest never answers. `hold` encodes it,
and the `receive` shim registers it with the host when the dispatch returns,
because the host cannot call into a guest that is gone. If the guest is dropped,
or its actor closes while the engine keeps running, the host sends the registered
reply for each debt still owed, on the request's chain, before it releases the
settlement. A replace carries the registration with the debt. An engine teardown
sends nothing, since every requester is closing too. Write the impl by hand next
to the kind, with a failure arm the caller can tell from a real answer:

```rust
impl HeldReply for MeshLoadResult {
    fn unanswered() -> Self {
        Self {
            ok: false,
            namespace: String::new(),
            path: String::new(),
            error: Some("mesh actor closed before the load answered".into()),
            warnings: Vec::new(),
        }
    }
}
```

## Where to read more

- The full end-to-end loop for a component — crate setup, the `#[actor]` block,
  `export!`, the wasm build, and loading it over MCP —
  [Writing a component](recipes/writing-a-component.md).
- How you write an actor at all — its lifecycle, handlers, and addressing by type
  — [The actor model](foundations/actor-model.md).
