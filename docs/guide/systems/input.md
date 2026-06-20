# Input streams

> **Governing ADRs:** [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md)
> (publish/subscribe routing), [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)
> (subscribers keyed by `KindId`). The model — subscribe by kind, fan out to
> every subscriber — is **stable**. This page covers the input interrupts
> (key, mouse, window-size). The per-frame `Tick` is a frame-lifecycle stage:
> a component subscribes it on `aether.lifecycle`, not here — the
> [frame lifecycle](lifecycle.md) owns it.

The substrate owns the window, the keyboard, and the mouse. The events they
produce — a key down, the cursor moving, a resize — reach actors as ordinary
mail through one mailbox, `aether.input`, on a publish/subscribe model. An actor
that wants a stream **subscribes** to it; the substrate fans each event out to
every subscriber. Nothing is pushed at an actor that didn't ask; a stream with
no subscribers is dropped at the source.

When you author a component, this is how it feels the world — you subscribe to
`Key` to react to keystrokes, to `MouseMove` to follow the cursor, to
`WindowSize` to track the viewport. When you drive over MCP, input normally
originates at the platform layer, but you can inject a synthetic event by mailing
its kind to `aether.input`, which fans it out to whoever subscribed.

## Why it exists

Several actors can want the same input at once. A gameplay actor and a debug
overlay might both watch keystrokes; a HUD and a renderer might both want resize
events. Routing input to a single
owner would force the extra listeners to fan out through that one — the
incidental coupling the mail-first design exists to avoid. So the substrate keeps
a subscriber *set* per stream and broadcasts to every member, the same shape
observation mail already uses to reach every attached session. There's no default
recipient and no focus primitive; a component that wants exclusive input gets it
by being the sole subscriber.

Streams are keyed by **`KindId`** ([ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)) — the same identifier the mail
wire, dispatch, and the SDK already use for every kind. One identifier space, so
adding a new input is a one-line kind declaration plus the line that emits it;
there's no parallel enum or bridge table to keep in sync.

## What it does

**One mailbox, a set of streams.** Everything addresses `aether.input`, owned by
the `InputCapability` actor — the sole owner of the subscriber table
(`KindId → set of mailboxes`). The platform-driven streams:

| Kind | Fires on |
|---|---|
| `aether.key` | a key press |
| `aether.key_release` | a key release (paired with `key` for hold-to-act) |
| `aether.mouse_move` | cursor movement |
| `aether.mouse_button` | a mouse-button press |
| `aether.window_size` | a resize |

**`Tick` lives on the lifecycle, not here.** The per-frame advance is the
substrate's frame-lifecycle state machine (`aether.lifecycle.tick`), so a
component subscribes the `Tick` stage directly on `aether.lifecycle` —
`ctx.actor::<LifecycleCapability>().subscribe::<Tick>()` (ADR-0082), the same
subscribe shape as the input streams below. `Key`, `MouseMove`, and the rest are
genuine input interrupts and stay on `aether.input`.

**Subscribe and unsubscribe are mail.** Control kinds on `aether.input`:

- `aether.input.subscribe_self { kind }` — subscribe the *sending* actor to
  `kind`. The cap reads the subscriber off the inbound's host-stamped `Source`
  (ADR-0083), so the caller names neither the kind id nor its own mailbox. This
  is the common form. Idempotent. Replies `aether.input.subscribe_result`
  (`Ok` / `Err { error }`); a sender with no local mailbox (an external session
  or another engine) gets `Err`.
- `aether.input.unsubscribe_self { kind }` — the reflexive unsubscribe twin.
- `aether.input.subscribe { kind, mailbox }` — add an *explicit* `mailbox` to
  the set for `kind`. The rare cross-mailbox form. Idempotent; same reply.
- `aether.input.unsubscribe { kind, mailbox }` — remove it. Idempotent; same
  reply.
- `aether.input.unsubscribe_all { mailbox }` — drop `mailbox` from every stream.
  No reply; this is what the component host fires on drop.

A named (`subscribe` / `unsubscribe`) subscribe is validated: the mailbox must
name a live, dispatchable actor — a dropped or unknown id is rejected with
`Err`. The reflexive (`*_self`) forms need no such check: the host-stamped
`Source` already names the live sending actor.

**Fan-out, and the empty case.** A driver pushes each platform event as a single
mail to `aether.input`; the cap then sends one copy per subscriber, carrying the
inbound mail's lineage so a trace shows the copies fanning out under one parent.
If a stream has no subscribers, the fan-out reaches no one and the event is
dropped before it's ever enqueued.

**Subscriptions are keyed by mailbox, so they belong to the component across
instances.** A `replace_component` keeps the same mailbox id, so the new instance
inherits the old one's subscriptions with nothing to redo. A `drop` is the end of
them — the component host mails `unsubscribe_all`, so a torn-down mailbox can't
keep receiving fan-out.

## How to use it

**From a component — subscribe in `wire`.** `init` can't send mail (its context
is resolver-only), so subscriptions go in the `wire` hook, which runs post-init
with mail allowed. Address the cap by type and name the stream as a type
parameter — the cap subscribes the calling actor:

```rust
fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
    let input = ctx.actor::<InputCapability>();
    input.subscribe::<Key>();
    input.subscribe::<WindowSize>();
}
```

To subscribe a *different* mailbox (the rare cross-mailbox case) use the named
form: `input.subscribe_for::<Key>(other_mailbox)`.

Then handle each stream as its kind, like any other mail:

```rust
#[handler]
fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) { /* react to a keystroke */ }
```

`aether-kit`'s `camera` export subscribes `WindowSize` this way to track the
viewport, and subscribes the `Tick` and `Render` lifecycle stages on
`aether.lifecycle` to advance and submit each frame. You don't unsubscribe on the
way out — the host clears your subscriptions when the component drops.

**From an agent over MCP.** Input originates at the platform layer, so the usual
way you see it is through a subscribed component's behavior or its logs. To drive
a component without a real keyboard — in a test, or on the headless chassis — mail
the event's kind straight to `aether.input` (`aether.key`, `aether.mouse_move`,
…) and the cap fans it out to subscribers just as a platform event would. The
per-frame `Tick` is not an input stream: it is a frame-lifecycle stage on
`aether.lifecycle`, advanced by the lifecycle driver, so you don't pump it
through `aether.input` by hand.

## How to extend or reuse it

- **A new input stream** is a new kind plus an emitter. Declare the kind, mark it
  an input, and have the platform driver send it to `aether.input`; the cap fans
  it out by `KindId` with no enum to widen and no bridge table to edit
  ([ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)). A single-window assumption holds today; a per-window
  stream would carry a window id.
- **Any actor can subscribe**, not just components — a native capability that
  needs the tick or a resize subscribes through the same mail. Subscription is
  reachability: what an actor receives is exactly the streams it asked for.

## Where to read more

- The publish/subscribe decision and the no-ownership rationale —
  [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md);
  keying by `KindId` —
  [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md).
- The `wire` hook, `init` versus `wire`, and writing handlers —
  [Components & lifecycle](components.md).
- What drives the per-frame `Tick`, and the frame stages around it — the
  [frame lifecycle](lifecycle.md).
- `KindId`, the fan-out lineage, and addressing by kind —
  [Mail, kinds & scheduling](mail-and-kinds.md).
