# ADR-0249: A Republish Wires the Successor and Unwires the Old Guest at Commit

- **Status:** Proposed
- **Date:** 2026-10-06

Settles the question [ADR-0247](0247-six-invariants-where-actors-meet-the-engine.md)
left open about [ADR-0241](0241-code-is-published-not-loaded.md) §7: which
lifecycle hooks a republish runs, on which guest, and which of them may refuse
it. Tracked by #7529. This ADR is text only; the engine change is #7529's.

Three terms are used throughout. The **old guest** is the wasm instance that
runs before a republish. The **successor** is the instance built from the new
module to replace it (the code calls it the candidate). The **point of no
return** is the moment the component host sends `Commit` to the members of a
group, after the successor module's publish has settled
(`finish_republish_publish`, `crates/aether-component/src/component/runtime/republish/mod.rs`).

## Context

A guest has five lifecycle hooks: `init`, `wire`, `unwire`
(`Lifecycle`, `crates/aether-actor/src/model/mod.rs`), and `on_dehydrate`,
`on_rehydrate` (`WasmActor`, `crates/aether-actor/src/wasm/mod.rs`,
[ADR-0101](0101-replace-hooks-on-ffiactor.md)). A birth runs `init` then
`wire`, and a close runs `unwire`. A republish runs a different set.

### What a republish runs today

Read at `6577e30d9`, in `WasmTrampolineState::prepare`, `start_candidate`,
`rehydrate_candidate`, `commit` and `abort`
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
  `saved.map_or(Ok(()), ..)`, so a successor of a guest that saved nothing
  runs `init` and no other hook.
- **An abort runs `wire` a second time on the old guest.** `reinstate` hands
  the old guest its own bundle through `on_rehydrate` and then calls
  `wire_guest`, because its `unwire` ran at prepare. A fault in that second
  `wire` aborts the substrate: no birth is in flight to fail with it.
- **`unwire` does not reach inline children.** The `unwire` export runs the
  entry actor's hook only; `despawn_inline_child` is the one path that runs a
  child's `unwire` (`crates/aether-actor/src/wasm/ctx/spawn.rs`).

### What that costs

A subscription made in `wire` survives a republish only because the engine
holds it against the mailbox, which a republish does not change. A component
whose `unwire` undoes its `wire` comes back with nothing: the old guest
unsubscribed at prepare and the successor never subscribes. A new version's
additions to `wire` never run on a live instance.

Three kit components show it, each read in the tree.

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

### Hooks that cannot say no, and one that fails silently

- `on_rehydrate` returns `()`. The engine waits on it before the point of no
  return, and it is where a successor learns that the old state does not fit
  it, yet the only way it can stop the republish is to trap
  (`call_on_rehydrate` propagates a trap, and `rehydrate_candidate` refuses
  with "on_rehydrate failed"). The hook `#[actor]` generates for a declared
  `type State` (`crates/aether-actor-derive/src/wasm_expand.rs`) handles a
  bundle that does not decode by logging a warning and starting fresh. A
  rebuilt inline child that fails its `init` or placement is skipped with a
  warning and the republish goes on without it.
- `on_dehydrate` returns `()`. Two framework paths refuse through it: a live
  held reply that was not saved (`DEHYDRATE_HELD_UNSAVED`,
  [ADR-0243](0243-typed-held-replies.md) §6) and a `save_state` the host
  rejected (`take_save_error`). Both refuse the republish and the old guest
  is reinstated with the state it saved.
- **A trap in `on_dehydrate` is logged and the republish goes on.**
  `Component::on_dehydrate`
  (`crates/aether-substrate/src/actor/wasm/component/lifecycle.rs`) records
  no error for a trap. The export saves state as its last step, so a trap
  before it leaves no bundle: the successor is not rehydrated, its inline
  children are not rebuilt, and the republish answers `Ok`. If another
  member then aborts the group, the guest that trapped is reinstated and
  runs more code.

### What ADR-0247 left open

ADR-0247 rule 3 says an actor's life is one fixed sequence and no path runs a
step twice or skips one; rule 5 says what wired, unwires. Its note on
ADR-0241 §7 records that a republish keeps a lifecycle of its own and leaves
open "whether a republish is a close followed by a birth, in which case rules
3 and 5 apply to it as written, or a third thing with a rule of its own".

## Decision

### 1. Every guest instance has one fixed sequence

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

Inline children follow the same rule. A child the successor rebuilds runs
`init`, `on_rehydrate` if its state was carried, and `wire`. The old guest's
`unwire` at commit reaches its resident children first, then the entry actor,
which is the order ADR-0247 rule 5 gives a close.

### 2. The steps of one member

**Prepare.** All of it happens before the point of no return, and any step
may refuse.

1. The member closes its inbox gate. Mail for the guest waits in arrival
   order (`forward_to_wasm`, `PreparedSlot::gated`).
2. The successor is instantiated with its outbox held and its load window
   open over the republish's code. `init` runs. The old guest has run no
   hook yet, so a failed `init` leaves it untouched.
3. The old guest runs `on_dehydrate`.
4. The correlation cursor, reply table and watches move to the successor.
   The save error and the carried-context check
   (`check_carried_contexts`) are read.
5. The successor runs `on_rehydrate`, if the old guest saved state.
6. The successor and each child it rebuilt run `wire`. Its outbox is still
   held, so nothing `wire` sends has left. The load window closes when
   `wire` returns.
7. The member answers `Ready`.

**Commit.** After the point of no return. No step can refuse.

1. The old guest runs `unwire`, children first. Its outbox is not held, so
   its mail leaves now, ahead of anything the successor sent.
2. The old guest is dropped.
3. The successor's held outbox is flushed, in the order it sent, and its
   staged inline aliases publish.
4. The successor becomes the live guest and receives the gated mail in
   order.

The order of 1 and 3 is the rule that makes a paired `unwire` and `wire`
correct: when the old guest's `unwire` releases what the successor's `wire`
makes again, the release reaches its recipient first and the mailbox ends up
holding it.

**Abort**, and a prepare that refused at its own step 3 to 6.

- The successor runs `unwire` if its `wire` ran and did not trap, with its
  outbox still held. Then its held outbox is discarded and it is dropped, so
  nothing it sent from `init`, `on_rehydrate`, `wire` or `unwire` leaves.
- The old guest takes back the reply table, cursor and watches, and gets back
  the state it saved through its own `on_rehydrate`. It runs no `wire`: its
  `unwire` never ran, so it is still wired.
- The old guest receives the gated mail in order.

**A close while prepared** (`close_guest`'s prepared arm, reached by engine
teardown; a drop of a member waits for the republish). The successor is
released as an abort releases it. The old guest takes the reply table back,
runs `unwire`, has each reply it still holds answered `unanswered`
(`answer_held_at_close`), and is dropped. It gets no `on_rehydrate` first: it
is closing, and `unwire` after `on_dehydrate` is the order every replaced
guest runs.

```rust
// main: close_guest, prepared arm
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

### 3. `wire` is safe to run again for the same mailbox

`wire` runs once per guest instance. Across republishes it runs more than
once for one mailbox, so an author writes it to be correct when what it sets
up is already standing.

- **Subscriptions, watches and route claims** are idempotent at the door that
  takes them, so `wire` repeats them freely. Two doors were read for this
  ADR: a lifecycle subscription is an insert into a set keyed by the
  subscriber (`crates/aether-lifecycle/src/subscribers.rs`), and a watch of a
  standing target answers the standing id (`ComponentCtx::watch`,
  `crates/aether-substrate/src/actor/wasm/component/ctx.rs`). The other
  doors were not audited here; one that is found not to be idempotent is a
  defect at that door.
- **A one-shot send in `wire` repeats on a republish.** An author who wants
  it once keeps a flag in saved state and checks it in `wire`.
  `on_rehydrate` runs before `wire` so that `wire` can read what was carried.
- **An inline spawn in `wire` of a name that is already resident answers with
  the resident child.** The successor's children were rebuilt before its
  `wire` runs, so a `wire` that spawns its children finds them. Nothing is
  initialised again. On `main` such a spawn runs a second `init` and replaces
  the resident child's box without its `unwire` (`install_inline_child`,
  `Registry::insert_child`).

A `wire` and `unwire` that are written as a pair need no guard: at commit the
old guest's release arrives before the successor's request (§2).

### 4. A successor whose `wire` fails refuses the republish

A successor whose `wire` returns an error or traps refuses the republish of
its group. The successor is dropped and the old guest keeps running. So does
a rebuilt child whose `wire` fails: no spawn call is waiting for a
`SpawnError::WireFailed`, so the republish is the requester that is told.

The two faults differ as they do at a birth (`WireFault`,
`crates/aether-substrate/src/actor/wasm/component/dispatch.rs`). A successor
whose `wire` returned an error is intact and wired in part, so it runs
`unwire` before it is dropped. A successor whose `wire` trapped runs no more
code.

### 5. A hook can refuse only while the engine is waiting on it

A hook can refuse only if the engine is waiting on it before the point of no
return. For a birth the point of no return is the birth going live. For a
republish it is `Commit`.

| Hook | Runs on | The engine waits before the point of no return | It returns an error | It traps |
| --- | --- | --- | --- | --- |
| `init` | a new guest, at a birth or a prepare | yes | the birth fails, or the republish is refused; the guest is dropped and runs nothing more | the same |
| `on_rehydrate` | the successor, at prepare | yes | the republish is refused; the successor is dropped | the same |
| `wire` | a new guest, at a birth or a prepare | yes | the birth fails, or the republish is refused; the guest runs `unwire` and is dropped | the birth fails, or the republish is refused; the guest is dropped and runs nothing more |
| `on_dehydrate` | the old guest, at prepare | yes | the republish is refused; the old guest gets back what it saved and keeps running | the republish is refused; see open question 2 for the guest |
| `on_rehydrate` | the old guest, after a refusal or an abort | no: there is nothing left to refuse | the substrate aborts ([ADR-0063](0063-fail-fast-on-abnormal-component-lifecycle.md)) | the substrate aborts, as it does today |
| `unwire` | the old guest at commit; any guest at its close; a successor that wired and lost | no | it returns nothing | the trap is logged and the drop proceeds |

`unwire` cannot refuse on any path. At commit the group has already won; at a
close the actor is going whatever the hook says; for a successor that lost,
the refusal has already been given.

Signatures:

```rust
// main
fn init(config: Self::Config, params: Self::Params, ctx: &mut Self::InitCtx<'_>) -> Result<S, Self::InitError>;
fn wire(state: &mut S, ctx: &mut Self::Ctx<'_>) -> Result<(), Self::InitError>;
fn unwire(state: &mut S, ctx: &mut Self::Ctx<'_>);
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>);
fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_, Self>, prior: PriorState<'_>);

// plan: on_rehydrate gains a result; the others keep their shape
fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_, Self>, prior: PriorState<'_>) -> Result<(), ActorInitError>;
```

Three rules follow from the table.

- **`on_rehydrate` answers no.** An `Err` refuses the republish with the
  message, as a failed `wire` does. A rebuilt child that cannot be restored
  (an unknown type tag, a placement the successor's module rejects, a failed
  `init`, a failed `on_rehydrate`) refuses the republish too, where today it
  is skipped with a warning. What the hook generated for `type State` returns
  when a bundle does not decode is open question 1.
- **A trap in `on_dehydrate` refuses the republish.** It is recorded as a
  refusal the way a held-unsaved result is, so no successor is ever built
  from a guest that failed to save.
- **The old guest's `on_rehydrate` on the abort path cannot refuse.** It is
  given the bundle it wrote itself. An error or a trap there aborts the
  substrate, as a trap there does on `main`.

### 6. Held replies

On `main` the old guest's `unwire` runs before its `on_dehydrate`, and the
comments in `rehydrate_candidate` and `take_pending_replies` give the reason:
both hooks "may still answer handles". Only `unwire` can. `Held::answer`
takes a `WasmCtx` (`crates/aether-actor/src/wasm/ctx/held.rs`), which
`unwire` has and `on_dehydrate`, whose ctx is `WasmDropCtx`, does not. So
today a guest may answer a held reply in `unwire` on its way out of a
republish, and `on_dehydrate` then finds it gone.

With `unwire` at commit that allowance ends. The rule is ADR-0243 §6 with no
exception: at a republish a held reply is saved and carried to the successor,
or the republish is refused. `on_dehydrate` runs first and refuses on a live
held reply that was not saved, as it does today.

Nothing in the tree relies on the allowance. A search of every `fn unwire`
body under `crates/` found no wasm component or fixture that answers a held
reply there, and `replace_held.rs` already pins the refusal
(`an_unsaved_held_reply_refuses_the_replace_and_the_old_guest_answers`).

At commit the old guest holds no reply. Every ticket it had was saved, and
the reply table moved to the successor at prepare. Its `unwire` therefore
answers none, and an answer it attempts for a saved ticket must be refused
and logged, never delivered: the successor owns that reply. How the host
treats an answer for a handle its table no longer holds was not read for this
ADR, and the implementation confirms it with a test.

### 7. The load window and the mail that arrives meanwhile

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

One limit remains, by inference from the order of the two mails. When a
paired `unwire` unsubscribes and the successor's `wire` subscribes again, the
publisher handles the two mails back to back but not atomically. An event it
publishes between them is not sent to the mailbox. A component that leaves a
mailbox-keyed subscription standing in `unwire` has no such window. For a
lifecycle subscription it loses nothing by that at a close: a closed
subscriber leaves every stage through its `MonitorNotice`
(`crates/aether-lifecycle/src/subscribers.rs`). Other publishers were not
read.

### Why the contract has this shape: hooks read as mail

The fallibility contract comes from reading each hook as a piece of
specialised mail from the engine to the guest. `init` and `wire` read as
requests: the engine sends them, waits, and takes an answer that may be no.
`on_rehydrate` on a successor and `on_dehydrate` on the old guest read the
same way, which is why they belong in the same column. `unwire` reads as a
tell: by the time it is sent the decision is made, nobody is waiting, and
there is no answer to give.

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
  and no reply surface. A handler takes one ctx type. A hook that is mail
  needs those differences expressed some other way.

### Relationship to existing ADRs

- **ADR-0247.** Its open question on ADR-0241 §7 is closed by §1: a
  republish is a birth of the successor and a close of the old guest on a
  mailbox that continues, so rules 3 and 5 apply to each guest instance as
  written. ADR-0247 is Proposed and is edited in place to say so. Its other
  open mechanisms are untouched.
- **ADR-0241 §7 and ADR-0243 §6.** The prepare, commit and abort steps
  change as §2 gives them, and a held reply at a republish follows §6 here.
  Both ADRs are Proposed and describe the code as built, so their text
  changes in place with the implementation. This PR leaves them as they are.
- **ADR-0101, ADR-0114, ADR-0016 §4.** Each is Accepted and states a piece
  of today's order: "runs `wire` again" on an abort (ADR-0101's amendment),
  "only fresh spawns fire `wire`" (ADR-0114's amendment), and a trap as the
  only refusal from `on_rehydrate` (ADR-0016). Each gains its amendment line
  when this ADR is accepted.
- **ADR-0063.** Unchanged. A trap where no other guest can take over still
  aborts the substrate.

## Consequences

### Positive

- A component's `wire` is the one place it sets up, on a birth and on a
  republish. A new version's additions to `wire` run on live instances.
- A republish that aborts leaves no trace: the old guest's `unwire` never
  ran, so nothing it would have released was released, and no hook runs
  twice on one instance.
- A successor that cannot set up says so and the republish is refused, where
  today `on_rehydrate` can only log and run degraded.
- A guest that fails to save its state can no longer be replaced by one that
  starts fresh.
- `aether.kit.bundle` survives a republish with its texture.

### Negative

- Every `wire` now has to be correct when run again for a mailbox. A one-shot
  send in an existing `wire` repeats on the next republish until its author
  guards it.
- A guest can no longer answer a held reply on its way out of a republish. It
  carries the reply or the republish is refused.
- Prepare is longer by the successor's `wire`, so the inbox gate stays closed
  longer and a slow `wire` delays the group.
- `on_rehydrate` changes signature on `WasmActor`, so every override changes.
  The change is mechanical.
- A republish that today succeeds with a warning (a child that was not
  rebuilt, a trap in `on_dehydrate`) is refused.

### Tests that flip

Each was read at `6577e30d9`. They pin the order this ADR replaces, and the
implementation rewrites them to pin the new one.

- `crates/aether-component/tests/harness_guest_watch.rs`,
  `a_clerk_that_watched_in_its_own_wire_reports_before_and_after_its_desks_republish`:
  asserts a rebuilt clerk's `wired` list is empty ("a rebuilt clerk runs no
  wire", added by #7525). The module doc's paragraph saying a republish runs
  no `wire` goes with it.
- The same file, `a_wire_watch_stands_through_aborted_republishes`: asserts
  `wired == vec![watch; aborts + 1]`, one rerun of `wire` per abort. An abort
  runs no `wire`, so the list holds one entry.
- `crates/aether-component/tests/republish.rs`,
  `an_abort_after_ready_reinstates_and_rewires_the_ready_member`: asserts the
  ready gate's wire count is 2 and the refusing peer is wired again.
- `crates/aether-component/tests/replace_rollback.rs`,
  `a_reinstated_guest_is_wired_again`: asserts one more `WireObserved` after
  the abort.
- `crates/aether-actor/src/wasm/ctx/tests/spawn.rs`,
  `reconstruct_does_not_run_wire`: its stated purpose, that a reload is not a
  first attach, no longer holds. Whether its literal assertion on
  `reconstruct_one_child` survives depends on where the implementation wires
  a rebuilt child.

New cover the implementation owes: a successor's `wire` runs once and its
mail leaves only at commit; a failed or trapped successor `wire` refuses the
group and the old guest's wire count is unchanged; the old guest's `unwire`
mail precedes the successor's `wire` mail at one recipient; a rebuilt child
is wired once and an old child is unwired; a trap in `on_dehydrate` refuses;
an inline spawn of a resident name in `wire` answers the resident child.

### Kit workarounds that go

- `MeshViewerState` and its `on_dehydrate` / `on_rehydrate` in
  `crates/aether-kit/src/mesh/mod.rs`.
- `ControllerState` and its `on_dehydrate` / `on_rehydrate` in
  `crates/aether-kit/src/camera/controller/mod.rs`.
- The `follow_window` call in `on_rehydrate` in
  `crates/aether-kit/src/camera/mod.rs`. The camera's saved pose, glide and
  extent stay, since they are state it carries.

### Neutral / forward

- `docs/guide/systems/components.md` describes the load window as
  `init` + `wire` and says a reinstated guest "runs `wire` a second time";
  both passages change with the implementation.
- Nothing is added to the delivered-mail path. The change is in the
  trampoline's prepare, commit, abort and close, and in the guest's `wire`
  and `unwire` exports reaching inline children.
- ADR-0247 rule 5's guest `unwire`, children first, is built by the same
  change, for a close as well as for a commit.
- This ADR moves to Accepted when #7529's implementation is in the code.

## Open questions

1. **What the generated `on_rehydrate` does with a bundle that does not
   decode.** The hook `#[actor]` writes for `type State` starts fresh with a
   warning today ([ADR-0113](0113-kind-typed-actor-state.md), read in `wasm_expand.rs`). With a
   result to return it can refuse the republish instead. Refusing means a
   change to a state kind's shape cannot be republished onto a live instance
   until its author writes a hand-written `on_rehydrate` that migrates or
   discards; starting fresh means state is lost with only a log line.
   Recommendation: refuse, and let an author who wants a fresh start say so
   in a hand-written hook.
2. **What becomes of an old guest whose `on_dehydrate` trapped.** The
   republish is refused either way (§5). The guest's store is wherever the
   trap left it. Reinstating it runs more of its code on that store;
   aborting the substrate treats it as a trap in a handler is treated
   (ADR-0063) and as `reinstate` already treats a trap in the old guest's
   `on_rehydrate`. Recommendation: abort the substrate.
3. **Whether an author can refuse from `on_dehydrate`.** The framework
   already refuses through it. An author has no way to: the hook returns
   `()`. The contract allows a result here, since the engine is waiting.
   Recommendation: give it `-> Result<(), ActorInitError>` in the same change
   as `on_rehydrate`, so every hook the engine waits on has the same shape.
4. **Mail the old guest sends from `on_dehydrate`.** `WasmDropCtx` can send,
   and the old guest's outbox is not held, so that mail leaves before the
   point of no return and an abort does not take it back. This is unchanged
   from `main`. Holding the old guest's outbox from prepare would close it,
   at the cost of a second held outbox per member.
5. **The order rebuilt children wire in.** At a birth a child's `wire`
   completes inside its parent's spawn call, so a spawn always answers with a
   wired child. To keep that on a republish, rebuilt children would wire
   first, in the order they were rebuilt (a parent before its descendants),
   and the entry actor last. Recommendation: that order.

## Alternatives considered

- **Leave the successor unwired and document `on_rehydrate` as the place to
  set up again.** This is today's behaviour. It needs a saved state for the
  hook to run at all, which is what the empty state kinds in the kit exist
  for, and it puts set-up in a hook that cannot refuse.
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
- **Make hooks mail kinds now.** It would give the fallibility contract by
  construction. The two open problems under "hooks read as mail" have no
  design, and the contract does not need them solved to be stated.
