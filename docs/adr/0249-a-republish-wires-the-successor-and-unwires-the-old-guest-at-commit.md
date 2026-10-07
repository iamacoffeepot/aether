# ADR-0249: A Republish Wires the Successor and Unwires the Old Guest at Commit

- **Status:** Proposed
- **Date:** 2026-10-06

Settles the question [ADR-0247](0247-six-invariants-where-actors-meet-the-engine.md)
left open about [ADR-0241](0241-code-is-published-not-loaded.md) §7: which
lifecycle hooks a republish runs, on which guest, and which of them may refuse
it. In doing so it states the whole lifecycle contract: every hook, what can
fail inside it, what its signature is, and what runs in every situation an
actor can be in. Tracked by #7529. This ADR is text only; the engine change is
#7529's.

Four terms are used throughout. The **old guest** is the wasm instance that
runs before a republish. The **successor** is the instance built from the new
module to replace it (the code calls it the candidate). The **point of no
return** is the moment the component host sends `Commit` to the members of a
group, after the successor module's publish has settled
(`finish_republish_publish`, `crates/aether-component/src/component/runtime/republish/mod.rs`).
A hook **refuses** when it returns an error; a **trap** is a guest panic or
fault, and for a native actor a panic.

Everything under "Context" was read in the tree at `6577e30d9` unless a
sentence says it was inferred or taken from another ADR.

## Context

A guest has five lifecycle hooks: `init`, `wire`, `unwire`
(`Lifecycle`, `crates/aether-actor/src/model/mod.rs`), and `on_dehydrate`,
`on_rehydrate` (`WasmActor`, `crates/aether-actor/src/wasm/mod.rs`,
[ADR-0101](0101-replace-hooks-on-ffiactor.md)). A native actor has the first
three. A birth runs `init` then `wire`, and a close runs `unwire`. A republish
runs a different set.

### What a republish runs today

From `WasmTrampolineState::prepare`, `start_candidate`, `rehydrate_candidate`,
`commit` and `abort`
(`crates/aether-component/src/trampoline/runtime/republish.rs`) and
`reinstate` and `close_guest`
(`crates/aether-component/src/trampoline/runtime/state.rs`).

```
// main
old guest:  live -> unwire (its mail leaves at once) -> on_dehydrate -> dropped at commit
successor:  init -> on_rehydrate (only if state was saved) -> live          (no wire)
abort:      successor dropped, no hooks; old guest: on_rehydrate -> wire (a second time)
```

- **The old guest's `unwire` runs at prepare**, before the group has decided.
  Only the successor's outbox is held (`hold_outbox`, called in
  `instantiate`), so what the old guest sends from `unwire` leaves at once
  and an abort cannot take it back.
- **The successor never runs `wire`.** Neither does an inline child it
  rebuilds: `reconstruct_inline_children`
  (`crates/aether-actor/src/wasm/inline/compose.rs`) runs `init` and
  `on_rehydrate` on each child and stops there
  ([ADR-0114](0114-inline-child-actors.md), amendment of 2026-07-08: "only
  fresh spawns fire `wire`").
- **`on_rehydrate` runs only when the old guest saved state.**
  `rehydrate_candidate` calls `call_on_rehydrate` through
  `saved.map_or(Ok(()), ..)`.
- **An abort runs `wire` a second time on the old guest.** `reinstate` hands
  the old guest its own bundle through `on_rehydrate` and then calls
  `wire_guest`, because its `unwire` ran at prepare. A fault in that second
  `wire` aborts the substrate.
- **`unwire` does not reach inline children.** The `unwire` export runs the
  entry actor's hook only. `despawn_inline_child`
  (`crates/aether-actor/src/wasm/ctx/spawn.rs`) is the one path that runs a
  child's `unwire`, and a child that despawns itself mid-dispatch skips it.

### What that costs

A subscription made in `wire` survives a republish only because the engine
holds it against the mailbox, which a republish does not change. A component
whose `unwire` undoes its `wire` comes back with nothing: the old guest
unsubscribed at prepare and the successor never subscribes. A new version's
additions to `wire` never run on a live instance.

Three kit components show it.

- `aether.kit.mesh` and `aether.kit.camera-controller` each declare an empty
  state kind (`MeshViewerState`, `ControllerState`) and save it in
  `on_dehydrate`. Both doc comments say why: "Saving it is what makes the
  replacement's `on_rehydrate` run." Their `on_rehydrate` then repeats the
  camera subscription their `wire` makes, and logs an error where `wire`
  would have failed the birth, because "a rehydrate cannot refuse".
- `aether.kit.camera` repeats `follow_window` in `on_rehydrate` for the same
  reason.
- `aether.kit.bundle` creates its texture in `wire` and destroys it in
  `unwire`, and overrides neither replace hook. A republish destroys the
  texture and never creates one. This follows from the code as read; it was
  not reproduced on a running engine.

### Failures that have no way to be reported

Two hooks can fail and return `()`. Their failures are reported by a trap, by
a log line, or not at all.

- **`on_rehydrate`.** A prior state that does not decode, a reference that no
  longer proves, an inline child that cannot be rebuilt. The only way the
  hook can stop the republish is to trap (`call_on_rehydrate` propagates it
  and `rehydrate_candidate` refuses with "on_rehydrate failed"). The hook
  `#[actor]` generates for a declared `type State`
  (`crates/aether-actor-derive/src/wasm_expand.rs`) logs a warning and starts
  fresh when the bundle does not decode. `reconstruct_inline_children` skips
  a child that cannot be restored, with a warning.
- **`on_dehydrate`.** A save the host rejects, a state that does not encode,
  a held reply left live and unsaved. `WasmDropCtx::save_state` and
  `save_state_kind` (`crates/aether-actor/src/wasm/ctx/drop.rs`) panic on the
  first two: "Panics if the host `save_state` import returns non-zero" and
  "Panics ... When `value` does not encode". The host records the first as a
  save error before the guest panics (`take_save_error`), so the republish is
  still refused. The third is a framework check that returns
  `DEHYDRATE_HELD_UNSAVED` ([ADR-0243](0243-typed-held-replies.md) §6).
- **A trap in `on_dehydrate` that records no save error is logged and the
  republish goes on.** `Component::on_dehydrate`
  (`crates/aether-substrate/src/actor/wasm/component/lifecycle.rs`) does
  nothing else with it. The export saves as its last step, so no bundle
  exists: the successor is not rehydrated, its inline children are not
  rebuilt, and the republish answers `Ok`. If another member aborts the
  group, the guest that trapped is reinstated and runs more code.

### What a trap does today, by where it lands

- A trap in a handler aborts the substrate
  ([ADR-0063](0063-fail-fast-on-abnormal-component-lifecycle.md);
  `deliver_to_guest`, `state.rs`).
- A trap in `init` fails the birth or refuses the republish, and the guest is
  dropped (`Component::instantiate`,
  `crates/aether-substrate/src/actor/wasm/component/instantiate.rs`; `prepare`
  puts the old guest back untouched).
- A trap in `wire` at a birth fails the birth and the guest is released
  without another call (`WireFault::Trapped`,
  `crates/aether-substrate/src/actor/wasm/component/dispatch.rs`: "no more of
  its code runs"; `WasmTrampoline::wire`).
- A trap in the old guest's `on_rehydrate` after an abort aborts the
  substrate (`reinstate`: "there is no other guest to fall back to").
- A trap in `unwire` is logged and the close goes on (`Component::unwire`).

### What ADR-0247 left open

ADR-0247 rule 3 says an actor's life is one fixed sequence and no path runs a
step twice or skips one; rule 5 says what wired, unwires. Its note on
ADR-0241 §7 records that a republish keeps a lifecycle of its own and leaves
open "whether a republish is a close followed by a birth, in which case rules
3 and 5 apply to it as written, or a third thing with a rule of its own".

## Decision

### 1. A hook that can fail returns an error; a hook that cannot returns none

A hook that can fail returns a result, and the returned error is the way it
reports failure. A hook that cannot fail returns nothing. A trap is a bug and
is never how a hook says no.

"Can fail" means the hook's caller is waiting on it and could act on a no. The
engine waits on a hook only before a point of no return: a birth going live,
or a republish's `Commit`. So the same test gives both columns of the table
below.

| Hook | Its ctx | What can fail inside it | `// main` | `// plan` |
| --- | --- | --- | --- | --- |
| `init` (wasm) | `WasmInitCtx`: the asset catalog and load window; no mail | the config does not decode; an asset is missing; the author's own construction | `-> Result<Self, ActorInitError>` | unchanged |
| `init` (native) | `NativeInitCtx` | the author's own construction | `-> Result<S, BootError>` | unchanged |
| `on_rehydrate` (wasm) | `WasmCtx`: mail, resolve, inline spawn, held replies | the prior state does not decode; a reference no longer proves; a child cannot be rebuilt | `()` | `-> Result<(), ActorInitError>` |
| `wire` (wasm) | `WireCtx`: `WasmCtx` plus the load window | a reference does not prove; an asset is missing; an inline spawn fails | `-> Result<(), ActorInitError>` | unchanged |
| `wire` (native) | `NativeCtx` | the same kinds of failure | `-> Result<(), BootError>` | unchanged |
| `on_dehydrate` (wasm) | `WasmDropCtx`: `save_state`, and today one send verb | the host rejects the save; the state does not encode; a held reply is live and unsaved | `()` | `-> Result<(), ActorInitError>` |
| `unwire` (wasm, native) | `WasmCtx` / `NativeCtx` | nothing its caller could act on | `()` | unchanged |

```rust
// main
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>);
fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_, Self>, prior: PriorState<'_>);
fn save_state(&mut self, version: u32, bytes: &[u8]);                 // panics when the host refuses
fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K);     // panics when the value does not encode

// plan
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError>;
fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_, Self>, prior: PriorState<'_>) -> Result<(), ActorInitError>;
fn save_state(&mut self, version: u32, bytes: &[u8]) -> Result<(), ActorInitError>;
fn save_state_kind<K: Kind>(&mut self, version: u32, value: &K) -> Result<(), ActorInitError>;
```

- **`on_rehydrate` gains a result** because decode, resolve and rebuild all
  fail, and the engine is waiting.
- **`on_dehydrate` gains a result** because all three of its failures are
  real and two of them are reported by a panic today. The save verbs return
  the error so the hook passes it on with `?`. The held-unsaved check becomes
  an `Err` the export returns, in place of the `DEHYDRATE_HELD_UNSAVED` code.
- **`unwire` keeps `()`.** It runs when the decision is already made: at
  commit, at a close, or on a successor that has already lost. Its caller
  can do nothing with a no. The fallible calls inside it (`resolve`,
  `despawn_inline_child`, `unwatch`) hand their results to the author, who
  skips what is already gone; the send verbs return nothing.
- **The `type State` accessors keep their shapes.** `dehydrate(&self) -> State`
  and `rehydrate(&mut self, State)` do not fail. The hooks `#[actor]`
  generates around them return the save's result and the decode's: `Ok`
  when nothing was carried, and an error when bytes were carried and do not
  decode.
- **The erased hooks return the same result.**
  `ErasedWasmActor::erased_on_dehydrate` and `erased_on_rehydrate`, which a
  multi-actor module and every inline child are driven through, return
  `Result<(), ActorInitError>` and forward the hook's.
- **A dehydrate stops at the first hook that returns an error.** The parent's
  hook runs, then each resident inline child's. A hook that said no ends the
  step: running the remaining children would move more state out of a guest
  that is about to keep running.
- **The export saves what was captured, then returns the error.** The bundle
  holds what the hooks that ran saved. A child the walk did not reach is
  absent from it, and so is the refusing child when it saved nothing, so the
  reinstated guest's rebuild leaves both resident. The export returns the
  first error of the hooks, the host save and the held-unsaved check, in
  that order.
- **A rebuild returns its first failure.** `reconstruct_inline_children`
  stops at the parent's `on_rehydrate` error, a child that cannot be rebuilt,
  or a child whose recorded parent never became resident, and each child's
  error names its subname and the cause.

### 2. A guest that traps runs no more code

One rule covers every trap, and it turns on whether the engine can do without
the guest that trapped.

- **The guest was going to be discarded, or can be.** It is dropped without
  another call, and whatever was waiting on it fails. This is a newborn guest
  in `init` or `wire`, a successor in `init`, `on_rehydrate` or `wire`, and
  any guest in `unwire`.
- **The guest is the live one and has to keep running.** The substrate
  aborts (ADR-0063). This is a guest in a handler, the old guest in
  `on_dehydrate`, and the old guest in `on_rehydrate` after an abort.

Each case but one is today's behaviour, cited in the Context. The one that
changes is the old guest's `on_dehydrate`: a trap there is a trap in the live
guest, and it aborts the substrate where today it is logged and the republish
goes on.

The host reads one fault type for `wire`, `on_dehydrate` and `on_rehydrate`
(`HookFault`): the hook returned an error and the guest is intact, or it
trapped and runs no more code. A state bundle the host cannot place in the
guest, one past the deliverable bound or one for a guest with no allocator,
counts as a returned error: no guest code ran, so the guest is intact.

A native actor differs in one way, and for a reason. A wasm guest lives in
a store the engine can throw away, which is what makes the first case
possible. A native actor lives in the engine's own memory, so there is
nothing to discard around a panic, and only the second case applies: a panic
in any native hook or handler takes the engine down. On a pool worker the
cycle runs under `catch_unwind` and a caught panic escalates to a fatal abort
(`crates/aether-substrate/src/scheduler/pool.rs`, citing ADR-0063); that
covers a handler, a close's `unwire`, and a post-seal `init` and `wire`,
which run inside the spawning handler. A composed root's `init` and `wire`
run on the boot thread with no `catch_unwind` around them
(`crates/aether-substrate/src/chassis/builder/native_actor_boot.rs`), so a
panic there unwinds boot. No native hook's panic is contained, and none
should be: this is the rule, not work to do.

### 3. Every guest instance has one fixed sequence

```
init -> on_rehydrate (if state was carried) -> wire -> live -> on_dehydrate (if replaced) -> unwire -> dropped
```

A birth and a republish run the same sequence; the two optional steps are the
only difference. A republish is a birth of the successor and a close of the
old guest, on a mailbox that continues. The mailbox, its name and route, its
reply table, its correlation cursor, its watches and every subscription held
against it are the actor's and pass from one guest to the next. ADR-0247
rules 3 and 5 apply to each guest instance as written.

```
// main
old guest:  live -> unwire (its mail leaves at once) -> on_dehydrate -> dropped at commit
successor:  init -> on_rehydrate (only if state was saved) -> live          (no wire)
abort:      successor dropped, no hooks; old guest: on_rehydrate -> wire (a second time)

// plan
old guest:  live -> on_dehydrate -> [commit] unwire -> dropped
successor:  init -> on_rehydrate (if carried) -> wire -> [commit] live
abort:      successor: unwire -> dropped; old guest: on_rehydrate from its own bundle, no wire
```

A native actor's sequence is `init -> wire -> live -> unwire -> dropped`. It
has no replace hooks and is never republished: a republish's members are
guest trampolines (`Member::control: ProtocolRef<GuestControl>`), and a
module that declares a boot is refused by the pre-checks
(`crates/aether-component/src/component/runtime/republish/precheck.rs`).

### 4. Every situation, hook by hook

Each table lists the hooks in the order they run. "Serves" names the ADR-0247
rule the row holds. In every table, a trap follows §2 and is listed only
where the outcome needs saying.

#### A birth of a guest: a load, a spawn, a boot entry

Every wasm door reaches one staging function (`stage_requested`, ADR-0247
rule 4), so there is one table.

| Step | Runs on | Can refuse | A refusal | A trap | Serves |
| --- | --- | --- | --- | --- | --- |
| The load window opens; `init` | the new guest | yes | the birth fails and its requester is told; the guest is dropped | the same | 3, 4 |
| The accept set registers; `wire` | the new guest | yes | the birth fails and its requester is told; the guest runs `unwire` and is dropped | the birth fails; the guest is dropped with no further call | 3, 5 |
| An inline spawn inside `wire`: the child's `init`, then the child's `wire` | the child | yes | the spawn answers `SpawnError::InitFailed` or `WireFailed` to the parent's `wire`, which decides; a child whose `wire` refused runs `unwire` and its alias is retired | a trap in the one wasm instance: the row above | 3, 5 |
| The load window closes when `wire` returns; the guest is live | | | | | 3 |

Read: `WasmTrampoline::init` and `wire`
(`crates/aether-component/src/trampoline/runtime/mod.rs`), `wire_guest`,
`install_inline_child`. One gap is ADR-0247's and stays open there: a
`Publish` answers before its module's boot instance is born, so a boot whose
birth fails is only logged.

#### A birth of a native actor

| Step | Can refuse | A refusal | A panic | Serves |
| --- | --- | --- | --- | --- |
| `init` | yes | the birth fails and its requester is told; a composed root withdraws its mailbox claim and the boot fails | the engine goes down (§2) | 3 |
| `wire`, its mail held until the actor is activated | yes | the birth fails; the actor goes through the one `close`, which runs `unwire` and discards what both hooks sent; a composed root fails the boot, whose rollback closes it with its wired siblings | the engine goes down | 3, 5 |
| The contract publishes, the held mail is released, the actor is live | | | | 3 |

Read: `NativeActorBoot` (`chassis/builder/native_actor_boot.rs`: claim,
`init`, `wire`, then the dispatcher), `commit_directly`
(`actor/native/spawn/spawner/commit.rs`: `hold_outbound_for_activation`,
`A::wire`, and `close::<A>` with `SpawnError::WireFailed` on an error) and
`prepare.rs` (`A::init` on the calling thread), all under
`crates/aether-substrate/src`. The routes differ in when the name is
published, which is ADR-0247's open single stepped birth. The hook order,
the signatures and what a refusal does are the same on each.

#### A close: a drop, an engine teardown, a birth cancelled after `wire`

| Step | Runs on | Can refuse | A trap | Serves |
| --- | --- | --- | --- | --- |
| The mail still queued is drained | | | | 1 |
| `unwire`, inline children first and deepest first, then the entry actor | the guest | no | logged; the close goes on | 5 |
| Each reply the guest still holds is answered `unanswered`; engine teardown answers none | the host | | | 2 |
| The guest is dropped; each drop request is answered `Ok` | | | | |
| The name tombstones and each watcher is sent a `MonitorNotice` | the registry | | | 5 |

Read: `on_drop_component`, `WasmTrampoline::unwire`, `close_guest`,
`answer_held_at_close`. A slot whose guest was released by a trapped `wire`
runs none of the guest rows.

```rust
// main: the unwire export
<$component as Lifecycle<$component>>::unwire(instance, ..);      // the entry actor only

// plan
// each resident inline child's unwire, deepest first, then the entry actor's
```

An inline child that is despawned runs its own `unwire`, is dropped, and its
alias is retired and spent (`despawn_inline_child`). A child that despawns
itself mid-dispatch skips `unwire` today; under rule 5 it runs `unwire` when
its dispatch returns, before its box drops.

A native actor's close is the one `close`
(`crates/aether-substrate/src/actor/native/slot/close.rs`), which takes the
actor by value and has no variant: drain the residual inbox, `unwire`, drop
the cost rows, discard mail still held for an activation that never landed,
settle the held replies (silently at engine teardown, `unanswered`
otherwise), end the name in the registries, drop the actor. Its four exits
are the actor asking to close, engine teardown, a birth cancelled after
`wire`, and a boot rolled back. Teardown closes the spawned instanced actors
before the composed roots (`spawner/teardown.rs`), and wakes an idle slot so
it too runs `unwire` (`chassis/builder/teardown.rs`). `unwire` cannot refuse,
and a panic in it takes the engine down (§2). A guest's trampoline is a
native actor, so the guest close table above is this sequence with
`close_guest` as its `unwire`.

Closing a parent does not close a child that owns its own slot. That is
ADR-0247's open question about what a closing actor owns, and stays there.

#### A republish, prepare: before the point of no return

| Step | Runs on | Can refuse | A refusal | A trap | Serves |
| --- | --- | --- | --- | --- | --- |
| The inbox gate closes; mail for the guest waits in order | the member | | | | 1 |
| The load window opens over the republish's code; `init`, outbox held | the successor | yes | the republish is refused; the successor is dropped; the old guest has run no hook | the same | 3 |
| `on_dehydrate` | the old guest | yes | the republish is refused; the old guest gets back what it saved and keeps running | the substrate aborts | 2, 3 |
| The cursor, reply table and watches move; the carried contexts are checked | the member | yes | as the row above | | 2 |
| `on_rehydrate`, if state was saved; inline children are rebuilt inside it | the successor | yes | the republish is refused; the successor is dropped | the same | 3 |
| `wire`, outbox still held; rebuilt children wire as §6 gives | the successor | yes | the republish is refused; the successor runs `unwire` and is dropped | the republish is refused; the successor is dropped with no further call | 3, 5 |
| The load window closes when `wire` returns; the member answers `Ready` | | | | | |

Whenever the republish is refused after the old guest's `on_dehydrate` ran,
the old guest is put back by the "abort" table below.

#### A republish, commit: after the point of no return

| Step | Runs on | Can refuse | A trap | Serves |
| --- | --- | --- | --- | --- |
| `unwire`, children first; its mail leaves now | the old guest | no | logged; the commit goes on | 5 |
| The old guest is dropped | | | | |
| The held outbox is flushed in the order it was sent; staged inline aliases publish | the successor | | | 1 |
| The successor is the live guest and receives the gated mail in order | | | | 1, 3 |

The old guest's `unwire` mail leaves before the successor's held mail. That
order is what makes a paired `unwire` and `wire` correct: when the old guest
releases what the successor's `wire` asks for again, the release reaches its
recipient first and the mailbox ends up holding it.

```rust
// main: commit
candidate.flush_held_outbox(ctx);
drop(old);

// plan
old.unwire();                        // children first; not held, so it leaves first
drop(old);
candidate.flush_held_outbox(ctx);
```

#### A republish, abort: a refusal by this member or another, or a failed publish

| Step | Runs on | Can refuse | A refusal | A trap | Serves |
| --- | --- | --- | --- | --- | --- |
| `unwire`, if its `wire` ran and did not trap; outbox still held | the successor | no | | logged | 5 |
| The held outbox is discarded and the successor is dropped; nothing it sent from any hook leaves | the successor | | | | 1 |
| The cursor, reply table and watches move back | the member | | | | 2 |
| `on_rehydrate` with the bundle it saved, if it ran `on_dehydrate` | the old guest | yes | the instance closes by the close table; nothing is left to refuse | the substrate aborts | 3 |
| No `wire`: the old guest never ran `unwire`, so it is still wired | | | | | 3 |
| The old guest receives the gated mail in order | | | | | 1 |

The old guest's `on_rehydrate` returning an error is the one refusal with no
operation left to refuse. The guest is intact, as a guest whose `wire`
returned an error is (`WireFault::Returned`), and it has said it cannot take
its state back. It closes in order: `unwire`, its held replies answered
`unanswered`, its name tombstoned. A trap there aborts the substrate, as it
does today.

#### A close while prepared

`close_guest`'s prepared arm. A drop of a member waits for the republish
(`parked_drops`), so engine teardown is what reaches it.

```rust
// main
candidate.discard_held_outbox();
old.resume_replies(candidate.take_pending_replies());
old.answer_held_at_close();          // the old guest's unwire ran at prepare

// plan
candidate.unwire();                  // only if its wire ran and did not trap
candidate.discard_held_outbox();
old.resume_replies(candidate.take_pending_replies());
old.unwire();                        // children first
old.answer_held_at_close();
```

The successor is released as an abort releases it. The old guest gets no
`on_rehydrate` first: it is closing, and `unwire` after `on_dehydrate` is the
order every replaced guest runs. The gated mail drops and each chain
settles, as today; ADR-0247 rule 1's `discard` is what gives it a record.

#### What runs no hook

A process that is killed, crashes or aborts runs nothing (ADR-0247 rule 5
excludes it). A republish of identical bytes is a no-op and runs nothing
(ADR-0241 §7).

### 5. `wire` is safe to run again for the same mailbox

`wire` runs once per guest instance. Across republishes it runs more than
once for one mailbox, so an author writes it to be correct when what it sets
up is already standing.

- **Subscriptions, watches and route claims** are idempotent at the door that
  takes them, so `wire` repeats them freely. The table below is every such
  door a `wire` body in the tree uses, found by searching the bodies outside
  the fixtures, with what a second identical call from the same mailbox
  does.
- **A one-shot send in `wire` repeats on a republish.** An author who wants
  it once keeps a flag in saved state and checks it in `wire`.
  `on_rehydrate` runs before `wire` so that `wire` can read what was carried.
- **An inline spawn in `wire` of a name that is already resident answers with
  the resident child.** Nothing is initialised again. Residency is read from
  the guest's own registry, by the three things an alias is folded from: the
  spawning actor, the child's type, and the name. The host is not called, so
  the alias is not staged for publication a second time and the child's
  dependencies are not checked again. The config a repeat passes is ignored,
  as the `Spawn` door ignores it for a live name (ADR-0241 §9). A counter
  spawn is always a new name, and a name whose child was despawned is spent,
  not resident. On `main` such a spawn runs a second `init` and replaces the
  resident child's box without its `unwire` (`install_inline_child`,
  `Registry::insert_child`).

| Door | A second identical call | Read in |
| --- | --- | --- |
| `aether.lifecycle` subscribe | no change: an insert into a map keyed by the subscriber | `crates/aether-lifecycle/src/subscribers.rs` |
| `aether.window` subscribe | no change: the subscriber map and the holder's row set are both keyed inserts | `KindSubscribers::insert`, `crates/aether-window/src/runtime/subscribers.rs` |
| `aether.http` route claim | `Ok`, no change: "the same sole holder re-claiming its own key is an idempotent `Ok`" | `crates/aether-http/src/server/runtime/state.rs` |
| `aether.rpc` engine route | `Ok`, no change: "the same registrant re-registering its own engine is `Ok` and changes nothing" | `on_register_engine_route`, `crates/aether-rpc/src/server/runtime.rs` |
| `aether.kit.camera` view subscribe | held once; the camera sends its current view again | `Viewers::add`, `on_view_subscribe`, `crates/aether-kit/src/camera/` |
| `aether.render` view-from | keeps the hold and subscribes to the source again, which answers with its current view | `follow_view`, `crates/aether-render/src/runtime/view_source.rs` |
| `ctx.watch` | answers the standing id | `ComponentCtx::watch`, `crates/aether-substrate/src/actor/wasm/component/ctx.rs` |
| `aether.tcp` bind for the sender | **not idempotent**: each call spawns a fresh listener for the address | `on_bind_self`, `crates/aether-tcp/src/runtime.rs` |

The TCP bind is the one door that fails the rule, and making a second bind
of the same address by the same consumer answer with the standing listener
is implementation work under this ADR. Its only `wire` caller today is a
test. That a second bind then fails at the socket is inferred from the
handler's doc, not read in the socket code.

The other sends found in `wire` bodies are one-shot commands or queries
(`CreateTexture`, a workspace import, a window list request, a mesh load),
which the second bullet covers.

A `wire` and `unwire` written as a pair need no guard: at commit the old
guest's release arrives before the successor's request (§4).

### 6. Inline children wire in birth order and unwire in the reverse

At a birth a child exists only once something spawns it, and
`install_inline_child` runs the child's `init` and then its `wire` inside the
spawn call. `WasmInitCtx` has no spawn verb, so the earliest a child is born
is inside its parent's `wire`. Two facts follow: a child's `wire` never runs
before its parent's `wire` has started, and a spawn verb always answers with
a child that has wired.

A republish keeps both. The successor's children are rebuilt inside
`on_rehydrate`, parents before descendants (`reconstruct_inline_children`),
and are not wired there.

1. The entry actor's `wire` runs.
2. A spawn inside a `wire` that names a rebuilt child wires that child before
   it answers, which is the moment the child wired at its birth. The same
   holds one level down, inside the child's own `wire`.
3. When an actor's `wire` returns, each of its rebuilt children that is still
   unwired wires then, in rebuild order. These are the children a handler
   spawned after the birth; at the birth they too wired after their parent
   was wired.

A rebuilt child whose `wire` refuses or traps refuses the republish. So does
a child that cannot be rebuilt: an unknown type tag, a placement the
successor's module rejects, a failed `init`, a failed `on_rehydrate`. No
spawn call is waiting for a `SpawnError`, so the republish is the requester
that is told.

`unwire` runs the other way: every resident child before its parent, deepest
first, the entry actor last. That is the order ADR-0247 rule 5 gives a close,
and the order `despawn_inline_child` already uses for one child. A child's
depth is the number of recorded parent links between it and the entry actor.
Children at one depth run in the reverse of the order a republish rebuilds
them in, which is the registry's walk order, so `unwire` is the exact reverse
of the order step 3 wires rebuilt children in. A child that did not wire runs
no `unwire` at a close: a rebuilt child that has not wired yet, or one whose
`unwire` has already run. So each child runs `unwire` at most once, and a
parent whose own `unwire` despawns its children unwires none of them a second
time.

### 7. Held replies

On `main` the old guest's `unwire` runs before its `on_dehydrate`, and the
comments in `rehydrate_candidate` and `take_pending_replies` give the reason:
both hooks "may still answer handles". Only `unwire` can. `Held::answer`
takes a `WasmCtx` (`crates/aether-actor/src/wasm/ctx/held.rs`), which
`unwire` has and `on_dehydrate` does not. So today a guest may answer a held
reply in `unwire` on its way out of a republish, and `on_dehydrate` then
finds it gone.

With `unwire` at commit that allowance ends. The rule is ADR-0243 §6 with no
exception: at a republish a held reply is saved and carried to the successor,
or `on_dehydrate` returns an error and the republish is refused.

Nothing in the tree relies on the allowance. A search of every `fn unwire`
body under `crates/` found no wasm component or fixture that answers a held
reply there, and `replace_held.rs` already pins the refusal
(`an_unsaved_held_reply_refuses_the_replace_and_the_old_guest_answers`).

At commit the old guest holds no reply: every ticket it had was saved, and
the reply table moved to the successor at prepare. If its `unwire` answers a
saved ticket anyway, the host finds no row for the handle, returns
`REPLY_UNKNOWN_HANDLE` and sends nothing
(`reply_mail_p32`, `crates/aether-substrate/src/actor/wasm/host_fns.rs`). The
successor answers that reply once.

### 8. `on_dehydrate` saves and does not send

An aborted republish leaves no trace, and the old guest keeps running after
one. Mail sent from `on_dehydrate` would break both: the old guest's outbox
is not held, so the mail leaves before the point of no return and an abort
cannot take it back.

`on_dehydrate` is the hook that saves. The hook that announces a guest's
departure is `unwire`, which now runs at commit, when the departure is
certain. So `WasmDropCtx` loses its send surface.

```rust
// main
impl MailSender for WasmDropCtx<'_> { fn send_detached_to(..) }

// plan
// WasmDropCtx implements Persistence only
```

No `on_dehydrate` body in the tree sends: a search of every override outside
the SDK for a send verb found none. Holding the old guest's outbox from prepare was
the other way to keep the rule. It adds a second held outbox per member to
carry mail that has a better hook to be sent from.

### 9. The load window and the mail that arrives meanwhile

The successor's load window stays open until its `wire` returns, as a
birth's does (`wire_guest` closes it whether or not `wire` succeeded). The
bytes are at hand: `Prepare { code, config }` carries them to each member,
and `instantiate` opens the window over them. On `main` the window closes
before `on_rehydrate`. This does not depend on ADR-0247 rule 4's open
question about bytes at the door, which concerns a `Spawn` with no `code`.

No mail is lost between the old guest's last handler and the successor's
first. The gate queues what arrives while the member is prepared and the
winner receives it in order
(`mail_gated_during_prepare_reaches_the_winning_guest_in_order`,
`crates/aether-component/tests/republish.rs`). An event subscription keeps
publishing to the mailbox throughout, because its key is the mailbox, which
both guests share.

One case can miss an event, and it is stated here as a limit. When the old
guest's `unwire` unsubscribes and the successor's `wire` subscribes again,
the two mails reach the publisher in that order but as two separate
deliveries: each guest send is routed as its own mail
(`ComponentCtx::send`, `route`, `flush_held`), and the publisher's inbox
takes mail from other senders between them. An event the publisher emits
between handling the two is not sent to the mailbox. The engine has no
delivery that hands one recipient two mails as a unit, and this ADR does not
add one, because no door in the tree needs it:

- **A publisher that holds its rows against the mailbox and releases them
  when the subscriber closes** needs no unsubscribe in `unwire`, so there is
  no first mail and no window. Lifecycle, window, HTTP and RPC all release on
  the subscriber's `MonitorNotice` (each read: `subscribers.rs` in
  `aether-lifecycle` and `aether-window`, `state.watch` in `aether-http`, the
  departure handler in `aether-rpc`). The rule for an author: do not
  unsubscribe in `unwire` from a publisher that releases on close.
- **A publisher of state that answers a subscribe with its current value**
  loses nothing across the window: the successor's subscribe is answered
  with the latest value. The kit camera is this case: it answers a subscribe
  with its current view, and it also releases a viewer when the viewer
  closes. `aether.kit.mesh` and `aether.kit.camera-controller` still
  unsubscribe in `unwire`, which stays correct.

A publisher of events that neither releases on close nor replays would be
exposed. None exists in the tree, and one that is written should release on
close, which ADR-0247 rule 5 already asks of whatever holds a row for an
actor.

### Why the contract has this shape: hooks read as mail

The contract comes from reading each hook as a piece of specialised mail from
the engine to the guest. `init`, `wire`, `on_rehydrate` on a successor and
`on_dehydrate` on the old guest read as requests: the engine sends them,
waits, and takes an answer that may be no. `unwire` reads as a tell: by the
time it is sent the decision is made, nobody is waiting, and there is no
answer to give. A request has a reply and a tell has none, which is §1's
rule in other words.

This is the reasoning only. Hooks stay trait methods called through the
guest's exports. Making them literal mail kinds is out of scope and is a
possible later direction, with two problems that would have to be solved
first:

- **Where a hook sits relative to queued mail.** A mail kind is delivered in
  inbox order. `wire` must run before any queued mail, `unwire` after the
  residual drain, and `on_dehydrate` at a point the republish chooses. A hook
  that is mail needs a rule for its position that an inbox does not have.
- **What the special ctxs become.** `WasmInitCtx` has no send surface,
  `WireCtx` carries the load window, and `WasmDropCtx` carries `save_state`
  and nothing else. A handler takes one ctx type. A hook that is mail needs
  those differences expressed some other way.

### The six rules of ADR-0247, checked

- **Rule 1, every mail ends one way.** Gated mail is delivered to the winner
  or discarded with a record at a close while prepared. A successor's held
  mail is flushed or discarded whole. `on_dehydrate` sends none.
- **Rule 2, every request gets one answer.** A held reply is carried or the
  republish is refused; a close answers `unanswered`; the old guest's late
  answer is refused by the host, so no reply is sent twice.
- **Rule 3, one fixed sequence.** §3. No instance runs `wire` twice, and no
  instance goes live without it. This removes the two departures on `main`:
  the successor that never wires and the reinstated guest that wires twice.
- **Rule 4, every instance gets the same birth.** A successor's birth has the
  same hooks, the same load window and the same child order as a load's.
- **Rule 5, what wired, unwires.** The old guest at commit, a successor that
  wired and lost, both guests at a close while prepared, every inline child
  before its parent, and a child that despawns itself.
- **Rule 6, never refused for timing.** Mail waits at the gate; a load, spawn
  or drop of a republishing namespace waits for the republish (ADR-0241 §7).
  A second republish of a module in flight is still refused; that is
  ADR-0241 §7's and is not changed here.

### Relationship to existing ADRs

- **ADR-0247.** Its open question on ADR-0241 §7 is closed by §3: a
  republish is a birth of the successor and a close of the old guest on a
  mailbox that continues, so rules 3 and 5 apply to each guest instance as
  written. ADR-0247 is Proposed and is edited in place to say so. Its other
  open mechanisms are untouched.
- **ADR-0241 §7 and ADR-0243 §6.** The prepare, commit and abort steps
  change as §4 gives them, and a held reply at a republish follows §7 here.
  Both ADRs are Proposed and describe the code as built, so their text
  changes in place with the implementation. This PR leaves them as they are.
- **ADR-0101, ADR-0114, ADR-0016 §4, ADR-0113.** Each is Accepted and states
  a piece of today's contract: "runs `wire` again" on an abort (ADR-0101's
  amendment), "only fresh spawns fire `wire`" (ADR-0114's amendment), a trap
  as the only refusal from `on_rehydrate` (ADR-0016), and a generated hook
  that starts fresh when the state does not decode
  ([ADR-0113](0113-kind-typed-actor-state.md)). Each gains its amendment line
  when this ADR is accepted.
- **ADR-0063.** Unchanged, and applied to one more place: the old guest's
  `on_dehydrate`.

## Consequences

### Positive

- A component's `wire` is the one place it sets up, on a birth and on a
  republish. A new version's additions to `wire` run on live instances.
- A republish that aborts leaves no trace: the old guest's `unwire` never
  ran, it sent nothing from `on_dehydrate`, and no hook runs twice on one
  instance.
- Every failure a hook can have has a returned error to travel in. An author
  never has to trap, or log and run degraded, to say no.
- A guest that fails to save its state can no longer be replaced by one that
  starts fresh.
- `aether.kit.bundle` survives a republish with its texture.

### Negative

- Every `wire` now has to be correct when run again for a mailbox. A one-shot
  send in an existing `wire` repeats on the next republish until its author
  guards it.
- A guest can no longer answer a held reply on its way out of a republish. It
  carries the reply or the republish is refused.
- A change to a `type State` kind's shape refuses the republish, where it
  started fresh with a warning. An author who wants a fresh start on a
  mismatch writes both hooks by hand and returns `Ok(())` when
  `prior.decode_kind::<S>()` is `None`; `#[actor]` already refuses a
  hand-written hook beside `type State`
  (`tests/ui/rejects_state_with_manual_hook.rs`), so the choice is explicit.
- A republish that today succeeds with a warning (a child that was not
  rebuilt) is refused, and a trap in `on_dehydrate` aborts the substrate.
- Prepare is longer by the successor's `wire`, so the inbox gate stays closed
  longer and a slow `wire` delays the group.

### What the implementation touches

Counts are from a search of `crates/` at `6577e30d9`, outside the SDK's own
definitions and the derive's source, fixtures and compile tests included.

| Change | Sites |
| --- | --- |
| `on_dehydrate` gains `-> Result<(), ActorInitError>` | 14 overrides in 13 files |
| `on_rehydrate` gains `-> Result<(), ActorInitError>` | 14 overrides in 12 files |
| `save_state` / `save_state_kind` return a result | 15 call sites |
| `erased_on_dehydrate` / `erased_on_rehydrate` return a result | the trait, the macro's one impl, and 11 test impls in three files |
| `Persistence::save_state` on the second implementor, `CaptureCtx` | one impl and its two calls, in `crates/aether-actor/tests/state_framing_roundtrip.rs` |
| Generated hooks for `type State` | the one generator (`wasm_expand.rs`); the 24 files that declare the accessors change no source |
| `WasmDropCtx` loses `MailSender` | no caller |
| `wire` (wasm 68 overrides in 43 files; native 31 in 20) | no signature change |
| `unwire` (wasm 13 overrides in 11 files; native 15 in 15) | no signature change |

The overrides of the two replace hooks are in `aether-kit` (camera,
camera-controller, mesh), `aether-test-fixtures-bundle`,
`aether-test-fixtures-republish` and two derive compile tests. No shipped
component declares `type State`; every user of the generated hooks is a
fixture or a compile test.

Engine and SDK work, each from a section above:

- The trampoline's `prepare`, `commit`, `abort`, `reinstate` and
  `close_guest` (§4).
- `Component::on_dehydrate` reads a returned error and treats a trap as §2
  gives; `call_on_rehydrate` reads a returned error.
- The `wire` export wires rebuilt children (§6); the `unwire` export reaches
  children; a self-despawned child runs `unwire`.
- An inline spawn of a resident name answers the resident child (§5).
- The `on_dehydrate` and `on_rehydrate` exports return the hook's error the
  way the `wire` export does (`stage_init_failure`), and
  `reconstruct_inline_children` returns a failure in place of a warning.
- The load window stays open through `wire` (§9).

### Tests that flip

Each pins the contract this ADR replaces, and the implementation rewrites it
to pin the new one.

- `crates/aether-component/tests/harness_guest_watch.rs`,
  `a_clerk_that_watched_in_its_own_wire_reports_before_and_after_its_desks_republish`:
  asserts a rebuilt clerk's `wired` list is empty (added by #7525). The
  module doc's paragraph saying a republish runs no `wire` goes with it.
- The same file, `a_wire_watch_stands_through_aborted_republishes`: asserts
  `wired == vec![watch; aborts + 1]`. An abort runs no `wire`, so the list
  holds one entry.
- `crates/aether-component/tests/republish.rs`,
  `an_abort_after_ready_reinstates_and_rewires_the_ready_member`: asserts the
  ready gate's wire count is 2 and the refusing peer is wired again.
- `crates/aether-component/tests/replace_rollback.rs`,
  `a_reinstated_guest_is_wired_again`.
- `crates/aether-component/tests/component.rs`,
  `typed_state_decode_miss_boots_fresh`, with its fixture
  `aether-test-fixtures-stateful-reshaped`: the republish is refused and the
  old count stands.
- `crates/aether-actor/src/wasm/ctx/tests/spawn.rs`,
  `reconstruct_does_not_run_wire`: its literal assertion still holds, since a
  rebuilt child wires in the `wire` step and not in the rebuild. Its stated
  purpose, that a reload is not a first attach, does not.
- Every fixture that refuses a republish by trapping in `on_rehydrate`
  (`trap_on_rehydrate` in the republish and watch fixtures) keeps working,
  since a successor's trap still refuses. The refusals that should be errors
  move to a returned `Err`, and the message "on_rehydrate failed" that
  `republish.rs` and `replace_rollback.rs` match on is kept for both.

New cover the implementation owes: a successor's `wire` runs once and its
mail leaves only at commit; a refused or trapped successor `wire` refuses the
group and the old guest's wire count is unchanged; the old guest's `unwire`
mail precedes the successor's `wire` mail at one recipient; a rebuilt child
is wired once, at its spawn in `wire` or after its parent's `wire`; an old
child is unwired before its parent; an `on_rehydrate` and an `on_dehydrate`
that return an error each refuse; a trap in `on_dehydrate` aborts; a child
that cannot be rebuilt refuses; an inline spawn of a resident name in `wire`
answers the resident child; a close while prepared unwires both guests; the
old guest's `on_rehydrate` returning an error after an abort closes the
instance.

### Kit workarounds that go

- `MeshViewerState` and its `on_dehydrate` / `on_rehydrate` in
  `crates/aether-kit/src/mesh/mod.rs`.
- `ControllerState` and its `on_dehydrate` / `on_rehydrate` in
  `crates/aether-kit/src/camera/controller/mod.rs`.
- The `follow_window` call in `on_rehydrate` in
  `crates/aether-kit/src/camera/mod.rs`. The camera's saved pose, glide and
  extent stay, since they are state it carries; its hook returns `Ok(())`
  where it starts from the config on a mismatch, which is that author's
  explicit choice.

### Neutral / forward

- `docs/guide/systems/components.md` describes the load window as
  `init` + `wire` and says a reinstated guest "runs `wire` a second time";
  both passages change with the implementation, as does the lifecycle bullet
  in `CLAUDE.md`.
- Nothing is added to the delivered-mail path.
- This ADR moves to Accepted when #7529's implementation is in the code.

## Open questions

None of this ADR's own. Each question an earlier draft listed is settled
above from the code or from precedent: the generated `on_rehydrate` (§1), a
trap in `on_dehydrate` (§2), a result on `on_dehydrate` (§1), mail from
`on_dehydrate` (§8), and the order children wire in (§6).

Three things the tables point at are not lifecycle questions pending here.
Each is owned by ADR-0247, which records it as an open mechanism to be
settled by its own ADR or an amendment there; no issue is filed for any of
them yet.

- **A module boot's birth that fails with no requester to tell** (ADR-0247
  rule 3). The boot runs the birth table above and fails as it says. What is
  missing is how a `Publish` reply waits on that birth, which is a question
  about the publish door's reply.
- **Closing a parent's separately slotted children** (ADR-0247 rule 5). Each
  such child is an actor with its own slot and runs the close table when it
  closes. What is missing is who decides that it closes, which is a question
  of ownership between actors.
- **The single stepped birth** (ADR-0247 rules 3 and 4). The native routes
  run the same hooks in the same order with the same refusals, read above.
  They differ in when the registry publishes the name, which is registry and
  boot-seal work.

None of the three changes which hooks run, their order, their signatures, or
what a refusal or a trap does.

## Alternatives considered

- **Leave the successor unwired and document `on_rehydrate` as the place to
  set up again.** This is today's behaviour. It needs a saved state for the
  hook to run at all, which is what the empty state kinds in the kit exist
  for, and it puts set-up in a hook that could not refuse.
- **Run `wire` on the successor and keep the old guest's `unwire` at
  prepare.** An abort would then have to wire the old guest again, which is
  the second `wire` on one instance this ADR removes, and the old guest's
  `unwire` mail would still leave before the group had decided.
- **Run the successor's `wire` at commit.** `wire` could then not refuse,
  because the group has already won. A successor that cannot set up would be
  live and broken.
- **Skip `unwire` on the old guest, since the mailbox continues.** A new
  version may stop wanting something the old one set up, and only the old
  guest's `unwire` knows what that was. It also breaks ADR-0247 rule 5 for
  every replaced guest.
- **Make a republish a full close and birth of the actor.** The name would
  tombstone (ADR-0241 §8), held replies would be answered `unanswered`, and
  every reference to the instance would die. Keeping the mailbox is the
  point of a republish.
- **Keep the generated `on_rehydrate` starting fresh on a mismatch.** It
  loses state with a log line as the only sign. A carried context of a kind
  the successor does not declare already refuses the republish
  (`check_carried_contexts`), and saved state is the same case.
- **Reinstate an old guest whose `on_dehydrate` trapped.** It would run more
  code on a store left wherever the trap found it, which is what ADR-0063
  rules out for a handler.
- **Give `unwire` a result.** Its caller could only log it, which the author
  can do. A result nobody acts on reads as a way to refuse where there is
  none.
- **Wire every rebuilt child before the entry actor's `wire`.** A spawn would
  still answer a wired child, but a child would wire before its parent's
  `wire` had started, which never happens at a birth.
- **Make hooks mail kinds now.** It would give the contract by construction.
  The two open problems under "hooks read as mail" have no design, and the
  contract does not need them solved to be stated.
