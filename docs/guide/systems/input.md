# Input streams

> **Governing ADRs:** [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md)
> (publish/subscribe routing), [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)
> (subscribers keyed by `KindId`). The model — subscribe by kind, fan out to
> every subscriber — is **stable**. One stream, `Tick`, is also a frame
> lifecycle stage; what *drives its cadence* lives in the lifecycle state
> machine (its own page) — this page covers how you receive it and the rest.

The substrate owns the window, the keyboard, the mouse, and the per-frame clock.
The events they produce — a key down, the cursor moving, a resize, a tick —
reach actors as ordinary mail through one mailbox, `aether.input`, on a
publish/subscribe model. An actor that wants a stream **subscribes** to it; the
substrate fans each event out to every subscriber. Nothing is pushed at an actor
that didn't ask; a stream with no subscribers is dropped at the source.

When you author a component, this is how it feels the world — you subscribe to
`Tick` to advance each frame, to `Key` to react to input, to `WindowSize` to
track the viewport. When you drive over MCP, input normally originates at the
platform layer, but you can inject a synthetic event by mailing its kind to
`aether.input`, which fans it out to whoever subscribed.

## Why it exists

Several actors can want the same input at once. A renderer, a physics step, and
a telemetry collector might all advance on `Tick`; a debug overlay might watch
keystrokes alongside the component reacting to them. Routing input to a single
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
| `aether.lifecycle.tick` | each frame — see below |

**`Tick` is a lifecycle stage you happen to subscribe to here.** Its kind name is
`aether.lifecycle.tick`, not `aether.input.*`, because the per-frame advance is
the substrate's lifecycle state machine — that's what emits a tick each frame.
But you subscribe to it through `aether.input` exactly like the other streams,
and it fans out the same way, so from a component's seat `Tick` is just another
stream. The cadence behind it — when a frame advances, and what it waits on — is
the lifecycle page's subject, not this one.

**Subscribe and unsubscribe are mail.** Three control kinds on `aether.input`:

- `aether.input.subscribe { kind, mailbox }` — add `mailbox` to the set for
  `kind`. Idempotent (it's a set). Replies `aether.input.subscribe_result`
  (`Ok` / `Err { error }`).
- `aether.input.unsubscribe { kind, mailbox }` — remove it. Idempotent; same
  reply.
- `aether.input.unsubscribe_all { mailbox }` — drop `mailbox` from every stream.
  No reply; this is what the component host fires on drop.

A subscribe is validated: the mailbox must name a live, dispatchable actor — a
dropped or unknown id is rejected with `Err`.

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
with mail allowed. Address the cap by type and name the kind:

```rust
fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
    let me = MailboxId(ctx.mailbox_id());
    let input = ctx.actor::<InputCapability>();
    input.subscribe(Tick::ID, me);
    input.subscribe(WindowSize::ID, me);
}
```

Then handle each stream as its kind, like any other mail:

```rust
#[handler]
fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _tick: Tick) { /* advance a frame */ }
```

The reference `aether-camera` subscribes `Tick` and `WindowSize` this way and
advances its cameras on each. You don't unsubscribe on the way out — the host
clears your subscriptions when the component drops.

**From an agent over MCP.** Input originates at the platform layer, so the usual
way you see it is through a subscribed component's behavior or its logs. To drive
a component without a real keyboard — in a test, or on the headless chassis — mail
the event's kind straight to `aether.input` (`aether.key`, `aether.mouse_move`,
…) and the cap fans it out to subscribers just as a platform event would. The
per-frame `Tick` is the exception: its cadence is the lifecycle driver's job, not
something you pump by hand.

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
- What drives the per-frame `Tick`, and the frame stages around it — the frame
  lifecycle state machine (forthcoming).
- `KindId`, the fan-out lineage, and addressing by kind —
  [Mail, kinds & scheduling](mail-and-kinds.md).
