# ADR-0247: Six Invariants Where Actors Meet the Engine

- **Status:** Proposed
- **Date:** 2026-10-05

## Context

Five boundary bugs arrived close together:

- **#7460:** mail sent to an actor at the instant its inbox is dropped is destroyed without being counted. A debug build panics on the dropping thread, and in a release build the chain's root never settles.
- **#7461:** an instance spawned without its module's bytes gets a load window that serves no asset payload, so the same component is born differently depending on which door asked for it.
- **#7462:** a request discarded before a handler takes it settles its chain and sends nothing, so the requester's reply handler never runs and its stored context stays for the life of the actor.
- **#7463:** a guest whose `wire` hook traps is logged, and its load still answers `Ok`.
- **#7456:** a render program or texture array registered before the device exists was refused, where the device was only late.

Each was fixed or scoped at its own site. They are one question: what happens to a thing in flight (a mail, a request, a birth, a subscription) when the actor at the other end changes lifecycle state. The engine answers that question separately at every site where the two meet, and the sites disagree.

A read of the code at `5e4fa48cb` counted the sites.

**Mail.** A mail can end without a handler at 26 places in `aether-substrate` and `aether-component`, 19 of them reachable while the engine keeps running. Each place does its own subset of three things: record `Finished` so the root's in-flight count falls, discharge the envelope's obligation guard, and write a log line. `settle_discarded` (`crates/aether-substrate/src/chassis/ctx.rs`) does the first two for the relay seams and leaves the log to the caller through `RelayOutcome`; other sites call `record_finished` directly, some with a warn and some without. The envelope's own contract is a public pair, `discharge` and `mark_transferred` (`crates/aether-substrate/src/mail/registry/dispatch.rs`), checked by `ObligationGuard`, which is a debug-only tripwire. Five sites settle a mail and leave no record at all, and none writes a trace event that says the mail was discarded. The clearest case is a strict wasm component: a kind it has no handler for is settled as if handled, with no log on either side of the boundary.

**Requests.** ADR-0243 answers a request whose recipient closes while holding it: the ledger sends `HeldReply::unanswered()` (`crates/aether-actor/src/held_reply.rs`) to each live debt. That is one of three moments. A request that is discarded before delivery (unknown or dropped mailbox, a birth that was cancelled with mail parked behind it) or while queued (inbox drop, relay refusal, dispatch miss, decode refusal) gets no answer. Twelve discard sites can end a request that way, against two that produce the typed answer.

**Birth.** There are four native birth implementations, eight counting the wasm outer birth and its three inner ones, with three `init` call sites and four `wire` call sites. They run three different sequences: a composed root's route is `Live` before its `init` runs (`crates/aether-substrate/src/chassis/builder/native_actor_boot.rs`); a pre-seal instanced actor is `Live` in both registries before `wire` (`commit_directly`, `crates/aether-substrate/src/actor/native/spawn/spawner/commit.rs`); a post-seal actor is reserved `Starting`, then initialised, wired, and promoted. `wire` returns `()` (`crates/aether-actor/src/model/mod.rs`), so it cannot fail a birth: a native `wire` that panics aborts the engine, and a guest `wire` that traps is logged by `WasmTrampolineState::wire_guest` (`crates/aether-component/src/trampoline/runtime/state.rs`) while the birth answers `Ok`. The bytes for a guest's load window ride `Spawn.code: Option<Blob>` (`crates/aether-kinds/src/lib.rs`), which every door must remember to fill; the host "keeps none after a publish" (`crates/aether-component/src/component/runtime/spawn/mod.rs`).

**Close.** There are eight close implementations. Two run the full sequence of drain, `unwire`, cost rows, and the registry tail `finalize_close_and_fan_out`: the pooled dispatcher's close cycle (`DispatcherSlot`, `crates/aether-substrate/src/actor/native/slot/dispatcher.rs`) and `PumpedSlot::shutdown`, which writes the same steps again by hand. Six are partial. At engine teardown `PooledActorShutdown::shutdown_dyn` flags the slot and drops it with nothing left to wake it, so an idle pooled root is freed with no `unwire` and no registry close; `Drop for DispatcherSlot` documents this and settles the held replies silently. That covers every chassis capability. A guest's `unwire` runs only on `aether.component.drop`.

**Waiting.** The engine has one general wait: mail to a route that is `Starting` parks in the route's FIFO (`crates/aether-substrate/src/mail/registry/mailbox/birth.rs`) and is released in arrival order when the birth promotes. Past that point, "the recipient exists and is not ready" is each capability's own choice between a hand-written queue and an `Err` reply. Render queues two kinds in `AwaitingDevice` (`crates/aether-render/src/runtime/awaiting_device.rs`), added one at a time as each was noticed (#7456), and refuses a second `capture_frame` with "try again once the in-flight request completes" (`crates/aether-render/src/runtime/mod.rs`).

The pattern is the same on both sides. Where a guarantee is implemented per site, some sites omit a step. Where a sequence has several implementations, the side paths skip part of it. Fixing the five issues one at a time leaves the next site free to get it wrong.

## Decision

Six rules bind the boundary where actors meet the engine. Each rule is held by one mechanism: a single pathway that every case goes through. A rule is never held by code repeated at each site, and never by a nullable field or state flag that every method has to match on. Where the language allows it the mechanism holds the rule by construction: a consuming API whose only exits are the legal ones, a drop order, or a typestate.

Four mechanisms are decided here and are being built now. Four are not designed yet. For those, the rule is binding from this ADR and the mechanism is recorded as open, with the question that has to be answered first.

### 1. Every mail ends exactly one way

**Rule.** Every mail ends exactly one way: handled, or discarded with a record. A mail is never destroyed silently.

**Precisely.** "Handled" means a handler for its kind ran on the recipient. Every other end is a discard, whatever the cause: an unknown or dropped mailbox, a closed or closing inbox, a kind the recipient does not handle, a payload that does not decode, a birth that failed with mail parked behind it. A discard settles the mail's chain exactly once and leaves a record that names the recipient, the kind, the sender, and the reason. A mail that is counted and leaves no record breaks the rule, and so does a record that names the wrong reason.

**Mechanism (decided).** One `discard` on the `Mailer`, and two consuming ends on the envelope.

```rust
// main: each site does its own subset of {finish, discharge, log}
settle_discarded(&env, mailer);                        // finish + discharge; the caller logs
mailer.record_finished(mail.mail_id, mail.root);       // finish only; a log at some sites, none at others
env.discharge();                                       // public, paired by hand with mark_transferred

// decision: the envelope has exactly two ends, and each consumes it
env.finish(&mailer);            // the dispatcher tail, after a handler ran
env.discard(&mailer, reason);   // every other end
```

`Mailer::discard` is the only way to end a mail without a handler. It records `Finished`, writes one log line of a fixed `key=value` shape, and records a discard event in the trace, so a traced send shows a discard as a discard. The reason is a closed enum. The public `discharge` and `mark_transferred` pair goes away: an envelope can leave the type system only through `finish` or `discard`, so a new seam cannot settle a mail and skip the record. `Mail` values that wait outside an envelope (a parked FIFO, an owner command, an outbound buffer) sit in one container type whose drop discards what is left, which replaces the four hand-written drains.

This is one function reached from many seams. The engine has no single point every mail passes on its way out, and this ADR does not invent one. What holds the rule is the type: there is no third way to let go of an envelope.

Beside it, one inbox relay type. The four inbox-handler closures that today share one body through `relay_or_transfer`, and the two claim paths that build the same thing (`RelayInbox`, `DropOnShutdownClaim`), become one concrete relay type built in one place and one claim that returns it. `RelayOutcome` and its per-site warn strings go with them, because the record is `discard`'s job. The relay holds no rule by itself; it makes the inbox seam one call site of `discard`.

**Replaces.** About 25 enforcement sites across three unrelated mechanisms, `settle_discarded`, the per-site warns, and a debug-only guard as the only check that a seam did all three steps.

### 2. Every request gets exactly one answer

**Rule.** Every request gets exactly one answer: the reply, or a typed "not answered". This holds whenever the recipient died: before delivery, while the request was queued, or while the recipient was holding it.

**Precisely.** The answer is a value of the reply kind the requester asked for, delivered to the reply handler the requester already has, so its stored context is taken and freed. A log line is no answer, and neither is a settled chain. "Exactly one" forbids two answers as much as none. The rule covers every discard under rule 1 that ends a request, including a request to a mailbox that never existed and one of a kind the recipient does not handle: from the requester's side each is a request nobody will answer.

**Mechanism.** One of the three moments is decided and built: ADR-0243's ledger answers `HeldReply::unanswered()` when an actor closes while holding a debt. This ADR leaves that mechanism as it is.

The other two moments are **open**. A request that ends at `Mailer::discard` (rule 1) has no answer today. After rule 1 there is one function that sees every such end and knows the requester's reply address and correlation, which is the precondition for any single mechanism. What is not designed is where the typed value comes from. The discard site does not know the reply kind for the commonest case, an address nothing is registered at, and carrying the reply kind on every mail would charge the delivered path for a need of the discard path. The candidate is to build the value on the requester's side, where the reply kind is declared. That needs answers to these questions first:

- Can a send verb's bounds name its reply kind at compile time, or must the requester's framework find it from the stored request?
- Is the value `unanswered()` itself, or a sibling that also carries the discard reason?
- Does every kind a response handler takes then implement `HeldReply`, as held kinds do today?
- A request discarded when an inbox is dropped is discarded on whatever thread drops it. How is its answer sent from there without running handlers on that thread?

Until that design lands, the rule is binding on new code in one way: no new site may end a request by any route other than rule 1's `discard`, so the eventual mechanism has one place to live.

**Replaces.** Twelve discard sites that leave a request unanswered while the engine runs, and the typed "could not answer" shapes that individual capabilities invented for their own cases.

### 3. An actor's life is one fixed sequence

**Rule.** An actor's life is one fixed sequence: init, wire, live, closing, closed. A hook that fails fails the birth, and whoever asked is told.

**Precisely.** No actor is live before its `init` and `wire` have both succeeded. No path runs the steps in another order, skips one, or runs one twice. A failure in `init` or in `wire` ends the birth: the instance does not go live, what the birth had acquired is released, and the requester of the birth (a load, a spawn, a boot entry, an MCP call) receives the failure as its answer. An answer of `Ok` means both hooks ran and succeeded.

**Mechanism (decided for `wire`).** `wire` returns a result, and the birth owns it.

```rust
// main: wire cannot fail; the trampoline logs a guest trap and the birth answers Ok
fn wire(state: &mut S, ctx: &mut Self::Ctx<'_>) { .. }
if let Err(e) = component.wire(root) { tracing::error!(..); }

// decision: the birth takes the result and fails on Err
fn wire(state: &mut S, ctx: &mut Self::Ctx<'_>) -> Result<(), Self::InitError> { Ok(()) }
component.wire(root)?;
```

The birth that called `wire` takes the cancel branch it already has for a failed `init`, and the requester's answer names a wire failure. The guest shim returns its hook's result where it returns `0` today. A guest's load window still closes whether or not `wire` succeeded. A failed `wire` leaves a wired-in-part actor to be closed, which is rule 5's job: the birth's cancel enters the one close.

**Open.** Three things in this rule have no mechanism yet.

- **A single stepped birth.** The three sequences in the Context (a root `Live` before `init`, a pre-seal instance `Live` before `wire`, a post-seal instance stepped through `Starting`) remain until one birth, stepped the same way by every driver, replaces them. That is the largest change this ADR points at. It rewrites the boot pipeline and reaches the boot seal, and it is not designed.
- **Closing as a state.** The registry has no published "closing". Whether the sequence's fourth step becomes a route state that mail routing reads, or stays internal to the one close, is not decided.
- **A module boot's birth.** A `Publish` answers before the birth of the module's boot instance is decided, so a boot failure with no waiter is only logged. The rule requires the publisher to be told. How its reply waits on that birth is not designed.

**Replaces.** Four `wire` call sites with two opposite failure behaviours (abort for native, swallow for a guest), and a `LoadResult::Ok` that can describe an instance whose subscriptions never happened.

### 4. Every instance gets the same birth

**Rule.** Every instance gets the same birth, however it was asked for: a load, a spawn, a boot manifest entry, or an MCP tool.

**Precisely.** The door decides who is told the outcome. It decides nothing about what the instance can do while it is born. Two instances of one type born through different doors run the same hooks in the same order with the same resources available to them: the same load window with the same assets, the same dependency check, the same registrations. Verifying a component through one door is then evidence about every door.

**Mechanism.** Part of this already holds: every wasm door reaches one staging function (`stage_requested`, `crates/aether-component/src/component/runtime/load.rs`), and every native birth after the seal goes through the registry owner.

Two parts are **open**.

- **Bytes at the door.** `Spawn.code: Option<Blob>` is the nullable state this ADR's direction rules out: a door that passes `None` gives its instance a load window that serves nothing, and each new door has to remember the field. The `Option` goes. Which way it goes is not decided. Either the publication keeps the bytes it was published from and every birth opens its window from the publication, which reverses ADR-0241 §2's choice to let the bytes go and costs memory per published module; or the field becomes required, which costs a caller across RPC a re-send per spawn unless it can name a blob the engine already holds.
- **The single stepped birth** of rule 3, which is also what removes the native differences: whether mail sent during a birth parks or queues, whether an instanced actor's published handles are kept, and whether a root appears in the actor registry.

**Replaces.** Four native birth implementations, and a per-door difference that #7461 and #7464 each fixed for the doors they touched.

### 5. What wired, unwires

**Rule.** What wired, unwires. Close always runs the full sequence, including for an idle actor at engine teardown.

**Precisely.** If an actor's `wire` ran, its `unwire` runs before the actor is gone, on every exit: the actor asked to close, the engine is tearing down, its birth was cancelled after `wire`, or boot rolled back. The full sequence is drain, `unwire`, release of what the engine keeps for the actor, and the registry tail that tombstones the name and notifies watchers. A process that is killed, crashes, or aborts runs nothing, and that is outside the rule: the rule is about every exit the engine itself takes.

**Mechanism (decided).** One `close`, entered from every exit.

```rust
// main: two full sequences and six partial exits
DispatcherSlot close cycle          // drain, unwire, cost rows, tail
PumpedSlot::shutdown                // the same steps written again; nothing runs if it is not called
PooledActorShutdown::shutdown_dyn   // flag, no wake; Drop settles held replies and runs no hook
WasmTrampoline                      // the guest's unwire runs only on aether.component.drop

// decision: one function owns the sequence and consumes the actor
close(actor, home, reason)          // drain, unwire, release, tail
// reason: the actor asked | engine teardown | birth cancelled | boot rolled back
```

`close` takes the actor by value, so an actor cannot be closed twice and cannot be dropped around it. The reason is data the one sequence reads, for the two places exits differ: held replies at engine teardown stay silent (ADR-0243 §1), and a birth that never reached the registry has no tail to run. Roots are retained through teardown the way instanced actors are, so teardown signals, wakes, and awaits them, after the instanced actors. A pumped slot enters `close` from its drop. A guest's `unwire`, children first, runs whenever its trampoline closes while the guest is live, and `aether.component.drop` becomes a request to close.

**Replaces.** Eight close implementations, of which two run the full sequence, and a teardown that frees every chassis capability without its `unwire`.

Two cases stay **open** under this rule. A component closed with its own requests outstanding cannot release what the late replies create, because `unwire` ran before they arrived; closing has no step that waits for what the actor asked for. Closing a parent does not close the children that own their own slots. Both need an account of who owns what, which is not designed.

### 6. A request is never refused for bad timing

**Rule.** A request is never refused for bad timing. If the recipient exists and is not ready, the request waits and is answered.

**Precisely.** "Not ready" means the recipient will be able to serve the request later without the requester doing anything: it is being born, a resource it needs has not arrived, or it is busy with an earlier request of the same kind. A reply that amounts to "try again" breaks the rule, because it hands the engine's sequencing problem to every caller. A refusal for a reason that waiting cannot change (the request is malformed, the thing asked for does not exist on this chassis) is a real answer and is not covered.

**Mechanism: open.** The one general wait is the `Starting` route's parked FIFO, and it covers birth only. Past birth there is no engine mechanism, so each capability writes its own queue or replies `Err`. The mechanism this rule needs is a single way for a live recipient to say "not yet" to one mail and have the engine keep it, in order, until the recipient says it is ready. No such design exists. The questions it must answer:

- Does a waiting mail block later mail of other kinds to the same recipient? The two hand-written queues today disagree.
- Does a waiting request keep its chain open, so the requester's own settlement timeout applies?
- What does close do with mail that is still waiting? Rules 1 and 2 say it is discarded with a record and each request is answered "not answered".

**The known doubt.** Waiting must not hide a recipient that never becomes ready. A refusal is at least visible; a request that waits forever on a device that will never come up is a hang with no message. Today no place could bound or report such a wait, because every queue is private to its capability. Whatever the mechanism is, exactly one place owns that policy: whether a wait is bounded, how a long wait is reported, and what the requester receives when the wait ends without readiness. A design for rule 6 that leaves this to each recipient has not met the rule.

Until the mechanism exists, the rule binds new code as a direction: a new request path that can arrive early queues the request and answers it, and never replies "try again".

**Replaces.** Per-capability queues and three known timing refusals.

### Relationship to existing ADRs

This ADR amends none of the following. It records where it constrains future work under them and where it leaves them alone.

- **ADR-0243 (typed held replies).** Its ledger and `HeldReply::unanswered()` are rule 2's mechanism for a recipient that dies while holding a request, unchanged. Its §1 choice that engine teardown settles held debts silently stands: at teardown every requester is closing too, and rule 5's `close` keeps that behaviour through its reason. ADR-0243 rejected "a generic `RequestAbandoned` notice to the caller" because the caller's handler for `R` would not run. That rejection stands and constrains rule 2's open mechanism: whatever answers an undelivered request must reach the requester as a typed `R` through the handler it already has. A generic notice that user code has to handle is still ruled out.
- **ADR-0230 (proven actor references).** Its §3 refuses a spawn or republish whose declared dependency has no `Live` route. That is a refusal for timing when the dependency is on its way up. This ADR does not decide whether rule 6 covers it. The refusal stays as ADR-0230 states it until that is decided, and the single stepped birth has to settle what "live" means for roots that boot together in any case.
- **ADR-0241 (code is published).** §8, an instance ends by closing, is the sequence rule 5 makes universal. §2, the bytes are not retained after a publish, is one of the two options rule 4 leaves open and stays in force until that choice is made. §7's republish keeps a lifecycle of its own: the old guest is unwired at prepare and the candidate that replaces it does not wire. Whether a republish is a close followed by a birth, in which case rules 3 and 5 apply to it as written, or a third thing with a rule of its own, is open, and §7 stands meanwhile. §9's doors are the doors rule 4 names.
- **ADR-0165 (handlers read views, emit effects).** Its boot seal divides births into direct pre-seal writes and post-seal births through the owner. Rules 3 and 4 hold today on the post-seal side. The single stepped birth would run pre-seal births through the same steps and so touches the seal; this ADR does not change it.
- **ADR-0079 (`init` / `wire` / `unwire`).** The hooks keep their meaning. `wire` gains a result.

## Consequences

### Positive

- A boundary bug of this family has one place to be fixed, and a new seam, door, or exit cannot get the rule wrong, because the only API it can call is the one that holds it.
- A discard is visible as a discard in the log and in the trace. A chain that settled because its mail was thrown away no longer reads as a chain that completed.
- `Ok` from a birth means the instance wired. Capability and guest `unwire` bodies run on every engine-driven exit, so what they release is released.
- The duplicated bodies go: the four inbox closures and two claim paths, the hand-written drains, `settle_discarded`, `PooledActorShutdown`, and the second close sequence in `PumpedSlot::shutdown`.

### Negative

- `wire` changes signature on the `Actor` trait, so every override changes, natively and in guests. The change is mechanical.
- The cancel-after-`wire` path gets real traffic for the first time, so `wire` returning a result and the one `close` land together.
- Root and guest `unwire` bodies that have never run at teardown now run. Some will send mail to peers that are closing in the same pass, and that mail is discarded with a record under rule 1. Teardown takes one more cycle per root, and a guest `unwire` that traps at teardown must not block exit.
- On the desktop chassis the pumped window and render roots close before actors that depend on them. One close makes that order visible, and it needs a decision.
- A discard caused by a bug or a hostile sender can repeat without bound, so the one log line needs a latch per episode, as the owner queue's has.
- Four mechanisms are open. Until they land, rules 2, 4, and 6 and the rest of rule 3 are true as direction for new code and false at the sites the Context lists.

### Neutral / forward

- The decided mechanisms add nothing to the delivered path: every call to `discard` is on a path that already ends the mail, and `close` runs once per actor.
- Follow-on design, in dependency order: the requester-side answer for an undelivered request (after `discard`); bytes at the door; a module boot as a requested birth; the wait mechanism and its one policy owner; the single stepped birth; an account of what a closing actor owns.
- #7460 is fixed at its site by PR #7465, and the one inbox relay follows that fix. #7463 is closed by `wire` returning a result. #7462 is rule 2's open mechanism. #7461 is rule 4's bytes at the door for the doors #7464 did not reach.
- This ADR moves to Accepted when the four decided mechanisms are in the code. Each open mechanism is recorded by its own ADR or by an amendment here when it is designed.

## Alternatives considered

- **Keep fixing each site.** This is what produced the five issues. Every fix was correct and none stopped the next site from omitting a step.
- **A lint or CI scan that checks each seam does all the steps.** It verifies the convention and keeps it a convention. A consuming API removes the wrong program from the language, and a scan only reports it after it is written.
- **A lifecycle state field on the actor that every method matches on.** This makes each rule expressible and holds none of them: every method has to remember the match, and a new state silently falls through the old ones. `Spawn.code: Option<Blob>` is this shape and is the cause of four of rule 4's seven violations.
- **A single choke point every mail passes on its way out.** Mail ends on many threads and in several containers. Routing every end through one queue would put a hop on paths that today end in place, to gain what two consuming ends on the envelope already give.
- **Carry the reply kind on every mail, so `discard` can build the typed answer itself.** Eight bytes written at every send, tells included, plus a table from reply kind to its encoded "not answered" that does not exist for wasm-only kinds. That is a cost on the delivered path for a need of the discard path. Rule 2's mechanism stays open in preference to this.
- **A generic "request abandoned" notice delivered to user code.** ADR-0243 rejected it and this ADR keeps the rejection: the requester's handler for its reply kind would not run, and its context would stay stranded.
- **Leave `wire` infallible and treat a guest trap in it as fatal, as every other trap is.** This makes native and guest agree, by taking the engine down for one component's failed setup. A birth already has a failure answer, and `wire` is part of the birth.
- **Keep teardown fast by skipping close for idle roots.** This is today's behaviour. It makes `unwire` a hook that runs only for actors that happened to be busy, so nothing a capability releases there can be relied on.
- **Answer rule 6 with a documented "retry" convention.** Every caller would then write the same loop, and none of them would know when to stop. The doubt about a recipient that never becomes ready is not removed; it is copied into each caller.
- **Wait for the full design of all six mechanisms before recording the rules.** The four decided mechanisms depend on none of the open questions, and the rules are what the open designs have to satisfy.
