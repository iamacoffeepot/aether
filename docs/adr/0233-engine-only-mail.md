# ADR-0233: Engine-Only Mail

- **Status:** Proposed
- **Date:** 2026-09-24

Amends [ADR-0079](0079-instanced-actors-as-a-first-class-category.md) §8:
`MonitorNotice` is engine-only mail.

## Context

Some mail states something only the engine can know:

- "this actor departed" (`aether.actor.monitor_notice`);
- "this causal chain settled" (`aether.trace.settled`);
- "advance the lifecycle one step" (`aether.lifecycle.advance`);
- "present a frame" (`aether.render.frame`);
- "your offloaded task finished" (`TaskCompletionWake`).

The engine sends these from host code, pushing straight through the mailer:
`notify_departure` (`crates/aether-substrate/src/actor/monitor.rs`), the
settlement registry's `push_settlement_notice`, the chassis drivers'
`push_chassis_root_mail`, and `NativeBinding::wake_self`. Nothing stopped an
actor from sending the same kinds. A native actor could pass one to a typed or
raw send verb, a wasm guest could hand its id to `send_mail_p32` or
`reply_mail_p32`, and an MCP client could name it in `send_mail`,
`send_mail_traced`, or a `capture_frame` bundle. A forged `Settled` sent to the
RPC server could close someone else's wire call early; a forged `MonitorNotice`
is an engine statement arriving from a live actor.

These kinds form a class. The class belongs on the kind, declared once, with no
hand-kept list of kind ids in the substrate.

## Decision

### The class

A kind is **engine-only mail** when the engine proper is its sole producer:
host code that pushes through the mailer, never an actor's binding. An actor
never originates it. A kind declares the class with
`#[aether_data::kind(name = "…", engine_only)]` (or `#[kind(name = "…",
engine_only)]` on the longhand derive).

### The `ActorMail` marker

`aether_data::ActorMail: Kind` is a positive marker for mail an actor may send
or reply:

```rust
#[diagnostic::on_unimplemented(
    message = "`{Self}` is engine-only mail: the engine sends it, never an actor",
    label = "declared #[kind(engine_only)]"
)]
pub trait ActorMail: Kind {}
```

- The `Kind` derive emits `impl ActorMail` for every kind unless it declares
  `engine_only`.
- A hand-written `Kind` impl adds `impl ActorMail for T {}` itself when the kind
  is actor-sendable. Leaving it out fails closed: the kind cannot be sent.
- It is not sealed. The orphan rule already stops a crate from implementing it
  for a kind it does not own, and a seal would need an escape hatch for the
  derive.
- `cargo check`, clippy, and the IDE report a violation at the call site with
  the message above, because it is an ordinary unsatisfied bound rather than a
  post-monomorphization error.

### The bound rule

Every generic bound on a path where an actor emits mail takes `K: ActorMail` in
place of `K: Kind`, replacing the old bound rather than stacking a second one:
sends (flat and handle), replies (synchronous and deferred), forwards, fan-outs,
hand-offs, spawn bootstrap mail, self-wakes, and the send facades. The anchors
are `MailSender`'s generic methods, `OutboundReply::{reply, reply_to}`,
`SendableTo<R>: ActorMail`, `Target<K: ActorMail>`, `Pending<R: ActorMail>`,
`MailboxForward::forward`, and `impl<O: ActorMail> ReplyShape for O`, so a
handler that declares an engine-only reply fails at its declaration.

Bounds that name a kind an actor receives, subscribes to, stores (request
contexts, persistence), or decodes stay `Kind`, as do host-side helpers
(`Mailer::send_reply`, `InboundMail::reply`, the chassis drivers,
`aether_substrate::testing`).

### Storage kinds

The `Storage` derive emits no `ActorMail`. A storage value reaches mail only
through handle indirection, and its `encode_into_bytes` panics, so a typed send
of one is now a compile error rather than a run-time panic. A storage kind
cannot declare `engine_only`.

### The link-time list and the raw doors

When a kind declares `engine_only`, the derive also submits one
`aether_data::name_inventory::EngineOnlyKind { kind, name }` entry on native
targets. `TaskCompletionWake`, a hand-written substrate kind, submits its entry
by hand beside its impl. `aether_substrate::mail::boundary::is_engine_only`
folds the list once into a keyed set and answers for the doors that carry only a
raw `KindId`, where no bound can help:

| Door | Refusal |
| --- | --- |
| RPC `RpcServerState::handle_call`, first, before the engine forward and the recipient proof | a correlated call gets `ReplyEnd` `Err(RpcError::Other { reason: "<kind id> is engine-only mail" })`; an uncorrelated one warns |
| `boundary::accept` (the `send_mail_traced` and `capture_frame` bundles), before each recipient proof | the bundle fails with `engine-only kind "<name>" in <label>` |
| guest `send_mail_p32` | status `3`, nothing sent |
| guest `reply_mail_p32`, before the reply handle is taken | `REPLY_ENGINE_ONLY_KIND` (`5`), nothing sent |
| native `NativeCtx::send_envelope_tracked_to` / `send_envelope_detached_to` | a warning and `MailId::NONE`, nothing sent |

The typed native path pays no lookup. `KindDescriptor`, the hub handshake, and
the wire frames are unchanged.

### The host exemption

None of the engine's own senders crosses a guarded door: `notify_departure`,
`push_settlement_notice`, the chassis drivers' `push_chassis_root_mail`, the
harness pump and capture extension, and `NativeBinding::wake_self` all encode
through `Kind::encode_into_bytes` and push through the mailer.

### Guest-declared kinds

A wasm crate may declare `engine_only`; the derive then emits no `ActorMail`,
so the guest's own typed sends of it fail to compile. The `aether.kinds` custom
section is unchanged and no inventory exists on wasm, so the host enforces the
class at its raw doors only for kinds linked into the native binary. That covers
every kind the engine actually originates. A guest-only engine-only kind is
therefore inert beyond the guest's own compile check.

### Membership

A kind joins only if the engine proper is its sole producer. A kind a
capability sends through its binding stays ordinary; declaring it would first
need that send moved onto a host path.

| Kind | Producer | Verdict |
| --- | --- | --- |
| `aether.actor.monitor_notice` (`MonitorNotice`) | `notify_departure` | engine-only |
| `aether.trace.settled` (`Settled`) | settlement registry `push_settlement_notice` | engine-only |
| `aether.render.pre_settled` (`PreSettled`) | the same settlement bridge | engine-only |
| `aether.lifecycle.advance` (`LifecycleAdvance`) | desktop, headless, and harness drivers | engine-only |
| `aether.render.frame` (`Frame`) | desktop driver, harness pump, harness capture extension | engine-only |
| `aether.render.occluded` (`Occluded`) | desktop driver | engine-only |
| `TaskCompletionWake` | `NativeBinding::wake_self` | engine-only |
| `aether.lifecycle.quit` (`Quit`) | an application asking to quit is a legitimate actor request | ordinary |
| `aether.rpc.call_settled` (`CallSettled`) | the fleet proxy, a capability, through its held mailer | ordinary |
| `aether.mail.unresolved` (`UnresolvedMail`) | no producer yet | ordinary |
| `aether.registry.changed` (`RegistryChanged`) | a coalescible wake; a forged one only makes the receiver re-read the real inventory | ordinary |
| `SelfWake<K>` kinds | the capability's own helper | ordinary |
| capability-sent kinds (`Tick` and the stage kinds, window input, `aether.fleet.*`, `aether.tcp.session_closed`, `LifecycleAdvanceComplete`) | capabilities through their binding | ordinary |

## Consequences

- A typed send or reply of an engine-only kind is a compile error at the call
  site on both transports, with the diagnostic naming the class.
- Every raw-`KindId` door refuses the class by looking up its declaration, so
  a wire client, an MCP bundle, a wasm guest, or a native actor cannot forge a
  departure, a settlement, a frame, or a lifecycle step.
- A hand-written `Kind` impl must opt in to `ActorMail` to be sendable. The
  failure mode of forgetting is a compile error, not a silent hole.
- A typed send of a storage kind no longer compiles; none did successfully,
  since it panicked at run time.
- Adding a kind to the class is one flag on its declaration. Removing one needs
  its producers audited for an actor-side send first.
- Guest-local dispatch between one module's inline actors never reaches the
  host, so a raw `send_bytes` between cluster members reaches no door. The
  typed sends there are refused by the bound.

## Alternatives considered

- **An inline-const assertion on a `Kind::ENGINE_ONLY` const.** A
  post-monomorphization error: `cargo check`, clippy, and the IDE never show it,
  and every funnel would need a guard call instead of a bound.
- **A hard-coded `kind == MonitorNotice::ID` predicate.** Puts the rule in the
  substrate rather than on the kind.
- **A negative marker with a negative bound.** Stable Rust has no negative
  bounds, and a positive marker fails closed for hand-written impls.
- **Sealing `ActorMail`.** The derive would need an escape hatch into the seal;
  the orphan rule already confines implementations.
- **Refusing in the mailer's routing chokepoint.** The host's own notices use
  the same push, and the burst producer and component direct dispatch bypass
  `route_mail`.
- **A flag on `KindDescriptor` or in the `aether.kinds` section.** Changes the
  hub handshake and the guest section format for a property only native kinds
  need.
- **A runtime check on every native send.** The typed path is already refused
  by the type system; only the raw-kind verbs need a lookup.
- **A new `RpcError` variant.** A wire change; `Other` carries the reason, as it
  does for a dropped recipient.
