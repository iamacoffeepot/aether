# Design rules

This page is Aether's design rulebook: the rules that decide API shape,
invariants, actors and addressing, data on the wire, and mechanism choices.
Code style (unit names, chains and paragraphs, comments, module layout) is in
[`CLAUDE.md`](https://github.com/iamacoffeepot/aether/blob/main/CLAUDE.md);
this page covers design only.

## How the rulebook works

**Rules are append-only.** Each rule has a stable id, `R-0001`, `R-0002`, and
so on, numbered in the order rules are added, with a matching anchor
(`#r-0001`). An id is never reused or renumbered, and a rule's text is never
edited once it lands.

**A change is a new rule.** Changing a rule appends a new rule with the next
id, and the old rule gains one line, `- **Superseded by:** [R-NNNN](#r-nnnn)`,
the only edit an existing rule ever receives. The rule in force is the head:
start at any rule and follow its **Superseded by** references until a rule has
none.

**Later rulings go to a log.** A rule's **Settled** line lists the
rulings known when the rule was added. A later ruling is appended to the
[Rulings](#rulings) log at the end of the page, one line naming the rule it
follows. Every ruling on an open question lands here, as a new rule or a log
line, in the same pull request that applies it.

**`/settle` cites rules by id.** When `/scope` or `/adr` leaves open
questions, the `/settle` skill checks each question's factual premise against
the code, then answers with one certainty level:

| Level | When | Cites |
|---|---|---|
| Settled | a rule decides the question directly | the rule that decides it |
| Leaning | rules decide it by analogy | the rule it follows, plus the counter-rule or ruling that weighs against it |
| Open | rules conflict, or none applies | the rules in conflict, or states that none applies |

ADRs and plans link a rule by its id (`design-rules.md#r-0001`), which stays
valid for as long as the page exists; a reader follows the forward references
to the head.

Each rule has one shape:

```text
### R-NNNN: <the rule, as one imperative line> {#r-nnnn}

<what the rule requires, concretely>

- **Why:** <one sentence>
- **Settled:** <rulings known when the rule was added: #NNNN (what) or ADR-NNNN §N (what)>
- **Superseded by:** [R-NNNN](#r-nnnn)   (added only when a later rule replaces it)
```

A citation names where a question was decided. Several cited ADRs are
Proposed; the citation records the ruling, and the code on `main` says what is
built.

## APIs and doors

### R-0001: Give each operation exactly one valid way to perform it {#r-0001}

An API admits one correct sequence and one spelling. A multi-step operation
ends in a consuming step (`finish(self, result)`) that yields a type with no
public constructor, so a forgotten step does not compile. When one syntax form
can express every case, it is the only form, even where a shorter second
spelling would read better for the common case.

- **Why:** every second path is a place to forget a step or pick the wrong
  spelling, and only a runtime check catches it.
- **Settled:** #6583 (`export!` takes keyed entries only; the bare form was
  removed); ADR-0109 (the return type is the reply contract, with no separate
  annotation that could drift from it).

### R-0002: Close logic holes by construction and keep unsafe for undefined behaviour {#r-0002}

`unsafe` marks code whose misuse is undefined behaviour. A logic invariant,
such as "only the macro implements this trait" or "only the registry mints
this proof", is closed by a sealed trait, a private constructor, or a type
derived from the item it describes, never by requiring an `unsafe impl`.

- **Why:** `unsafe` as a speed bump for a logic rule dilutes the keyword and
  leaves the hole open to anyone who writes it.
- **Settled:** #6843 (`#[protocol]` coverage is sealed traits with no
  `unsafe`); #6842 (contract rows exist only where a handler does, closed
  without `unsafe`).
- **Superseded by:** [R-0036](#r-0036)

### R-0036: Close a logic hole by construction where code outside its crate can reach it {#r-0036}

A logic invariant that code outside the owning crate could break is closed by
a sealed trait, a private constructor, or a type derived from the item it
describes, never by requiring an `unsafe impl`. Code outside the crate
includes a caller of a public item, an implementor of a trait implemented
across crates, and a macro that expands in the author's crate. Plumbing that
only the crate's own code reaches is held by that crate's code and review: a
wrapper, seal, or authority token around it isolates nothing, and none is
added. `unsafe` stays reserved for code whose misuse is undefined behaviour.

- **Why:** a seal is worth its cost only where it stops code the crate does
  not own; inside the crate it guards nothing and adds a layer to read.
- **Settled:** #6843 (`#[protocol]` coverage is sealed traits with no
  `unsafe`); #6842 (contract rows exist only where a handler does, closed
  without `unsafe`); #6877 (a seal on an effect internal to the substrate's
  registry, closed not planned).

### R-0003: Answer contract questions with types, not runtime machinery {#r-0003}

Whether a target handles a kind, what it replies, and whether the sender
handles that reply are answered by marker types and trait bounds checked at
compile time. A design that needs injected tokens, per-actor obligation state,
serialized replies, or async machinery to enforce a contract is replaced by a
type that carries the contract.

- **Why:** a type costs nothing per send and cannot be skipped; runtime
  bookkeeping costs on every call and hides the bug until it fires.
- **Settled:** ADR-0231 §1 (the static reply check on typed sends); ADR-0231
  §2 (protocols are zero-sized types the trait solver checks); ADR-0227
  (reply contracts are type markers).

### R-0004: Check an untrusted value once where it enters {#r-0004}

A runtime check runs only at a trust boundary, where a value is decoded from
mail, config, saved state, or an operator, and runs there once. Nothing
downstream re-checks what the type system, the decode, or a one-time
admission already proved. A sender is responsible for the validity of what it
sends.

- **Why:** a claim that holds monotonically is discharged once; re-checking
  it per call adds cost and a second place for the rule to diverge.
- **Settled:** ADR-0230 §1 (a reference's claim is discharged once and never
  revalidated); ADR-0231 §3 (a protocol path is proven at its decode, and
  neither the RPC door nor `resolve` checks it again); ADR-0241 §4 (one static
  admission check at publish).

### R-0005: Expose operations as ctx verbs and keep machinery unreachable {#r-0005}

An actor reaches the engine through verbs on its ctx (`send`, `subscribe`,
`resolve`, `spawn`). The mailer, registry, binding, and raw constructors are
private or crate-private. No public accessor, provided trait method, or
generic `Into` sink lets an implementor turn its own answer into a capability.
Before adding a public item, ask whether a caller stuck on a problem could use
it to bypass an invariant; if so, gate it or do not add it.

- **Why:** any reachable door is eventually used by a caller that cannot find
  the intended path.
- **Settled:** ADR-0230 §4 (gate the eliminators; one gated mint);
  #6699 (`Mail` sealed and the remaining raw-mailbox doors made
  crate-private); #6356 (the remaining positional doors deleted).

### R-0006: Write sends and subscriptions as flat ctx verbs {#r-0006}

A send or subscription is one verb on the ctx with the target in the
turbofish: `ctx.send::<R>(&mail)`, `ctx.subscribe::<P, K>()`. There is no
handle chain such as `ctx.actor::<R>().send(..)`. A turbofish names only types
the caller chooses, never `_`; a signature that would force `_` takes the
inferred part as `impl Trait` instead.

- **Why:** the call site reads as what it does, and a `_` placeholder is noise
  the caller cannot act on.
- **Settled:** ADR-0232 §1 (flat verbs replace the handle; a turbofish never
  names `_`).

### R-0007: Declare author-facing definitions as Rust items or attributes {#r-0007}

Something authors write often (an actor, a protocol, a dependency) is a plain
Rust item, a trait impl, or an attribute on a real item: `#[protocol]` on a
trait, `#[actor(depends(R))]` on an impl. A function-like macro wrapping a
code-shaped DSL (`protocol! { .. }`) is not used for it. A private
`macro_rules!` inside a library, such as tuple impls, is fine.

- **Why:** invented syntax inside a bang macro hides real Rust from readers
  and tooling.
- **Settled:** ADR-0231 §2 (protocols are declared on a trait, never as a
  bang macro wrapping a DSL); ADR-0232 §2 (dependencies are an `#[actor]`
  attribute option).

### R-0008: Pass a named-field struct instead of a run of positional arguments {#r-0008}

A function whose calls are runs of `None`, `0`, and bare literals takes a
plain struct with named fields, with `Default` where most fields are usually
empty. A new `#[allow(clippy::too_many_arguments)]` is not added.

- **Why:** a positional call cannot be read without counting positions
  against the signature.
- **Settled:** #6701 (many-parameter substrate signatures take named-field
  structs); #6696 (render pass recording and pipeline builds take
  named-field structs).

### R-0009: Land a new FFI import or reference type only with a named consumer {#r-0009}

A wasm host import, a guest ABI function, or a new reference or proof type
lands in the same change as its first production caller. Surface added ahead
of its consumer, on the promise of later work, is removed.

- **Why:** a host import is permanent guest ABI, and an uncalled one is a
  door with no reason to exist.
- **Settled:** #6295 (`Recipient`, `me()`, `ctx.resolve`, and
  `resolve_live_p32` removed until a site needs them); ADR-0230 §1 (the
  general own-path verb lands with its first consumer).

### R-0010: Enforce a design rule with types and existing lints {#r-0010}

A rule is closed by the type system (sealing, visibility, `unsafe` where the
risk is undefined behaviour) and the existing lint set (`clippy.toml`
`disallowed-methods`). A new source-scanning script added to back up a
type-level rule is not.

- **Why:** each bespoke scanner is one more gate every contributor has to
  understand and reproduce, for a rule the compiler already carries.
- **Settled:** #6617 (hand-written `DependsOn` impls refused by the compiler,
  with no scanner added).

## Values and the wire

### R-0011: Make every representation valid by construction {#r-0011}

A kind or value with an invariant has private fields, one fallible
constructor, and a fallible decode that runs the same check. There is no
`validate()` step, `is_valid` flag, or builder that yields a maybe-valid
value, so no code path ever holds an invalid one.

- **Why:** a value that can be invalid needs a state machine to track whether
  it was checked, which is invented complexity.
- **Settled:** ADR-0230 §2 (`ErasedActorPath` has a fallible constructor and
  a fallible decode); ADR-0231 §3 (a `ProtocolPath<P>` that exists is valid);
  #6860 (typed paths come only from type constructors).

### R-0012: Keep a type whose invariant cannot cross serialization off the wire {#r-0012}

A type whose invariant is relative to a context (this engine, this session,
this registry) implements no `Serialize`, `Deserialize`, `WireEncode`,
`WireDecode`, or `Schema`. The form that crosses is a separate description
that claims nothing the decode cannot prove, and the receiver proves it again
on its side.

- **Why:** a decode cannot know where its bytes came from, and a structural
  check at decode is not the invariant.
- **Settled:** ADR-0230 §1 (proven references have no codec; a path crosses
  and is re-proven); #6272 (ADR-0230's references made unexportable).
- **Superseded by:** [R-0037](#r-0037)

### R-0037: Give every kind an explicit reach and never serialize a raw mailbox id {#r-0037}

A kind is placed on two axes. The first is whether its decode needs a decode
context: a kind is contextual or non-contextual. The second is its reach,
how far its bytes may travel, one of three levels, narrowest first:

- **Actor:** bytes the same actor encodes and decodes, such as a stored
  request context.
- **Engine:** mail between actors in one engine session. Verified references
  may travel here.
- **Wire:** another process, saved state, or the wire. Basic types and some
  special types travel here.

A kind's reach is the narrowest reach among its fields, derived through
marker traits its fields implement, with no per-kind flag or attribute. A raw
`MailboxId` has no reach: it crosses at no level. A description, an actor
path, crosses where a reference cannot, and the receiver proves it again on
its side.

This rule records a direction that #6894 implements; it is not built on
`main`. Until #6894 lands, no kind carries a reach, so proven references keep
no codec and a description is what crosses.

- **Why:** whether a proof survives the trip depends on where the bytes go,
  so the kind's fields state how far they can go rather than every proof
  being barred from every trip.
- **Settled:** ADR-0230 §1 (proven references have no codec; a path crosses
  and is re-proven); #6272 (ADR-0230's references made unexportable); #6894
  (the decode-context axis, the three reach levels, and a kind's reach the
  narrowest of its fields: the direction is decided and the mechanism is
  pending there).
- **Superseded by:** [R-0042](#r-0042)

### R-0042: Give every kind an explicit reach and keep a raw mailbox id off the wire {#r-0042}

A kind is placed on two axes. The first is whether its decode needs a decode
context: a kind is contextual or non-contextual. The second is its reach,
how far its bytes may travel, one of three levels, narrowest first:

- **Actor:** bytes the same actor encodes and decodes, such as a stored
  request context.
- **Engine:** mail between actors in one engine session. Verified references
  may travel here.
- **Wire:** another process, saved state, or the wire. Basic types and some
  special types travel here.

A kind's reach is the narrowest reach among its fields, derived through
marker traits its fields implement, with no per-kind flag or attribute. A raw
`MailboxId` has engine reach, so a kind that carries one never reaches the
wire; which kinds may carry one is the door rule's question
([R-0041](#r-0041)). A description, an actor path, crosses where a reference
cannot, and the receiver proves it again on its side.

This rule records a direction that #6894 implements; it is not built on
`main`. Until #6894 lands, no kind carries a reach, so proven references keep
no codec and a description is what crosses.

- **Why:** whether a proof survives the trip depends on where the bytes go,
  so the kind's fields state how far they can go rather than every proof
  being barred from every trip.
- **Settled:** ADR-0230 §1 (proven references have no codec; a path crosses
  and is re-proven); #6272 (ADR-0230's references made unexportable); #6894
  (the decode-context axis, the three reach levels, and a kind's reach the
  narrowest of its fields: the direction is decided and the mechanism is
  pending there); #6890 (closed: a `MailboxId` has engine reach, so a kind
  carrying one cannot reach the wire).

### R-0013: Model a closed set as a Rust enum {#r-0013}

When every meaningful value implies compiled code (a handler, a drainer, a
match arm), the set is closed and is a fieldless Rust `enum` with exhaustive
matches. A string or a newtype with associated constants is used only at a
genuine persistence or skew boundary, converted at that edge. Before
accepting "open set", find who can mint a value and what must exist at
compile time for it to mean anything.

- **Why:** an exhaustive match makes a new member a compile error at every
  site that must handle it.
- **Settled:** #3693 (Bloomery outbox topics became a total `Topic` enum,
  with strings only at the storage boundary).

### R-0014: Keep mailbox ids out of every serialized type and public API {#r-0014}

No type with a codec or schema carries a `MailboxId`, directly or
transitively: kind fields, config, saved or dehydrated state, journal
records, MCP and RPC payloads, and trace or log exports name actors by path.
Outside the substrate's own plumbing, no API takes or returns a `MailboxId`.
An exception is narrow, named, and justified in the decision that makes it.

- **Why:** a position is a hash of names, so nothing outside the engine can
  tell a registered position from a computed one.
- **Settled:** ADR-0230 §1 (no serialized type carries a `MailboxId`; existing
  positions are listed as debt); #6854 (`Address` and `AddressForm` deleted);
  #6846 (the remaining serialized positions).
- **Superseded by:** [R-0038](#r-0038)

### R-0038: Keep MailboxId out of every door outside the substrate {#r-0038}

No public, actor, or guest API takes or returns a `MailboxId`, and it never
serializes ([R-0037](#r-0037)). A test of actor behaviour names actors by
reference or path. Inside `aether-substrate`'s plumbing and its own tests, a
`MailboxId` is the registry's key and belongs there, with no wrapper or seal
around it ([R-0036](#r-0036)). The rule is applied by its intent: the question
is whether code outside the substrate could use the id as a door, not whether
the type appears in a signature.

- **Why:** a position is a hash of names, so outside the engine nothing can
  tell a registered position from a computed one; inside the registry it is
  the key the proofs are checked against.
- **Settled:** ADR-0230 §1 (no serialized type carries a `MailboxId`; a
  `MailboxId` is a registry key inside the engine); #6854 (`Address` and
  `AddressForm` deleted); #6846 (the remaining serialized positions); #6877
  (the substrate's own registry plumbing takes no seal); #6895 (sends go
  through typed proofs).
- **Superseded by:** [R-0041](#r-0041)

### R-0041: Keep MailboxId out of the public APIs other code is written against {#r-0041}

The core engine crates, `aether-data`, `aether-actor`, `aether-substrate`,
and their proc-macro crates `aether-data-derive`, `aether-actor-derive`, and
`aether-derive`, hold and use a `MailboxId` in their own machinery, internal
and public plumbing items alike: it is the registry's key, a routing input,
and a field of the core's own engine-reach kinds ([R-0042](#r-0042)). There
it takes no wrapper or seal ([R-0036](#r-0036)). The rule governs the public
APIs that capabilities, components, harnesses, MCP, and tools are written
against: an author-facing ctx verb, a kind they send or receive, anything
they serialize, and a test of actor behaviour outside the core crates. None
of these takes or returns a `MailboxId`; an actor there is named by a proof
or a path. The rule is applied by its intent: the question is whether code
written against the engine could use the id as a door, not whether the type
appears in a public signature.

- **Why:** a position is a hash of names, so code written against a public
  API cannot tell a registered position from a computed one; inside the
  core engine it is the key the proofs are checked against.
- **Settled:** ADR-0230 §1 (a `MailboxId` is a registry key inside the
  engine); #6854 (`Address` and `AddressForm` deleted); #6877 (the
  registry plumbing takes no seal); #6895 (sends go through typed proofs);
  #6890 (closed: the rule covers public APIs other code is written
  against, not the core engine's machinery); #6903 (the line is the core
  engine crates, not `aether-substrate`).

### R-0015: Hold proofs in stored state, never positions {#r-0015}

Actor state and capability tables that outlive a handler hold an
`ActorRef<R>`, a `ProtocolRef<P>`, or an `ErasedActorRef`, and tables are keyed
by the proof. A position that arrives in a payload is proven once, at receipt,
in the handler that received it, and the proof is stored. A proof's id is
never unwrapped as the argument of the send, monitor, or close it exists for.

- **Why:** a stored proof carries the fact that its target was checked; a
  stored position carries nothing.
- **Settled:** ADR-0230 §2 (a reference may be held in actor memory; a
  `MailboxId` is only a registry key inside the engine); #6312 (the window
  proves a subscriber at receipt and stores references); #6685 (reply-table
  and alias-route constructors sealed; tests send through proofs).
- **Superseded by:** [R-0039](#r-0039)

### R-0039: Hold a typed proof for every actor that stored state sends to {#r-0039}

Actor state and capability tables that outlive a handler hold an
`ActorRef<R>` or a `ProtocolRef<P>` for every actor they will send to, and
tables are keyed by the proof. They never hold an `ErasedActorRef` for that
purpose, because erased actor sending is being removed (#6895). An
`ErasedActorRef` is kept only where nothing is sent through it: comparing
identity, keying a table, naming its canonical path, monitoring, and
ADR-0231 §4's cast, which types a reference that arrived untyped. A position
that arrives in a payload is proven once, at receipt, in the handler that
received it, and the proof is stored. A proof's id is never unwrapped as the
argument of the send, monitor, or close it exists for.

- **Why:** a stored typed proof carries both the check that its target was
  live and the kinds it may be sent; an erased one carries only the first.
- **Settled:** ADR-0230 §2 (a reference may be held in actor memory; a
  `MailboxId` is only a registry key inside the engine); ADR-0231 §4 (an
  erased reference has no send verb); #6312 (the window proves a subscriber
  at receipt and stores references); #6685 (reply-table and alias-route
  constructors sealed; tests send through proofs); #6895 (no send goes
  through an erased reference).

## Actors, names, and addressing

### R-0016: Describe an actor outside its handler only by an actor path {#r-0016}

Any description of an actor that leaves a handler (a mail field, config, a
journal record, the wire) is an `ErasedActorPath`, an `ActorPath<R>`, or a
`ProtocolPath<P>`. There is no second description type, such as an address
enum with position forms, beside them.

- **Why:** two description systems conflict, and the second one tends to
  smuggle positions into serialized types.
- **Settled:** ADR-0230 §2 (`ErasedActorPath` is the only actor description
  with a wire format); #6847 (actors are described only by actor paths);
  #6854 (`Address` deleted).
- **Superseded by:** [R-0040](#r-0040)

### R-0040: Carry a path its receiver will send to as a typed path {#r-0040}

Any description of an actor that leaves a handler (a mail field, config, a
journal record, the wire) is an `ErasedActorPath`, an `ActorPath<R>`, or a
`ProtocolPath<P>`, with no second description type beside them. The paths
mirror the references: `ErasedActorPath`, `ActorPath<R>`, and
`ProtocolPath<P>` pair with `ErasedActorRef`, `ActorRef<R>`, and
`ProtocolRef<P>`.

A path carried in a kind or config that its receiver will later send to, such
as a subscriber, a handler, a callback, or a source, is a typed path, never an
`ErasedActorPath`. It is an `ActorPath<R>` when the holder needs one concrete
actor type, and a `ProtocolPath<P>` when the holder needs only a protocol,
such as a subscriber the publisher cannot name. The receiver proves it once,
on receipt: a `ProtocolPath<P>` by its contextual decode, an `ActorPath<R>` by
its decode's leaf-namespace check. `ctx.resolve` then makes either one live,
and the proof it returns is what the receiver stores ([R-0039](#r-0039)).

An `ErasedActorPath` names, renders, compares, or monitors an actor. It also
names a recipient at the untyped boundary, such as an MCP tool argument or an
RPC `Call` recipient, which the boundary delivers through its private
stand-in (ADR-0231 §4).

The typed doors are not all built on `main`. The native `resolve` takes only a
`ProtocolPath<P>` (`crates/aether-substrate/src/actor/native/ctx/address.rs`),
and its `ActorPath<R>` arm lands with its first native caller (ADR-0230 §3).
A guest has no typed-path door yet: its `ProtocolPath<P>` decode lands with
ADR-0241, and its `resolve` over an `ActorPath<R>` lands with #6829.

- **Why:** an erased path proves to an erased reference, which has no send
  verb, so a path meant for sending carries the type its sends need.
- **Settled:** ADR-0230 §2 (the paths mirror the references); ADR-0231 §3
  rule 3 (a link is typed by what its holder needs); #6847 (actors are
  described only by actor paths, and protocol paths link them); #6854
  (`Address` deleted); #6863 (contextual kinds decode only with a context);
  #6895 (no send goes through an erased reference); #6896 (a path field its
  receiver sends to is a typed path).

### R-0017: Keep one addressing grammar {#r-0017}

Every string address at every boundary (MCP, RPC, CLI, config, manifests)
uses the ADR-0166 actor-path grammar, including its short form (`root/:key`).
No parallel URI, scheme, or resource grammar is introduced; a resource is
addressed as a root namespace and a lineage walk.

- **Why:** two grammars over the same strings make every reader guess which
  parser applies, and a wrong guess silently mis-addresses mail.
- **Settled:** ADR-0166 §5 (one grammar of `/`-separated steps); #6375 (the
  `://` short form retired for a hole in the canonical grammar).

### R-0018: Address a peer by its type marker {#r-0018}

A sender reaches a peer through the peer's type: a declared dependency
(`ctx.send::<R>`), a held reference, or a typed path. Roles, aliases, claims,
config slots naming a peer, and hardcoded mailbox-name strings are not
addressing mechanisms. Substituting an implementation means referencing the
new type under its own namespace.

- **Why:** a name string bakes the receiver's placement into the sender, so
  a moved receiver drops mail with no compile error.
- **Settled:** #5728 (role addressing declined); #5964 (`send_to_named`
  banned outside the routing core).

### R-0019: Name an actor by what it is, never by what hosts it {#r-0019}

An actor's path comes from its own type's namespace, cardinality, and
placement declaration, and never encodes its host's type. An actor that can
be embedded makes no compile-time assumption about which host carries it;
where parenthood matters, it is a runtime fact the host supplies.

- **Why:** a host pinned at compile time mis-addresses as soon as the
  topology changes.
- **Settled:** #4479 (the parent scope is carried in the ctx at runtime);
  ADR-0241 §5 (actors are named by what they are, on either runtime).

### R-0020: Tombstone a closed actor's name and never reuse it {#r-0020}

An actor ends by closing, and its name is retired for the engine's lifetime.
Mail to it drops; a later spawn of it is refused (`SubnameRetired`). No verb
frees a name for reuse, and no generation counter is added to the id.

- **Why:** a name that can return makes every held reference and path
  ambiguous about which actor it meant.
- **Settled:** ADR-0079 §7 (names are tombstoned on close, never reused);
  ADR-0241 §8 (an instance ends by closing, and its name tombstones).

### R-0021: Let contracts only grow across replace and republish {#r-0021}

A replacement may add contract rows and may not drop or change one; a
predecessor's `#[fallback]` counts as a row. A namespace, once published,
stays published for the engine's lifetime, and a republish exports every
namespace its predecessor did.

- **Why:** a monotone contract keeps every reference, typed path, and
  compiled assumption in a peer true across replaces.
- **Settled:** ADR-0231 §5 (replace preserves contracts); ADR-0241 §3 (a
  published namespace stays published); ADR-0241 §4 (contract growth checked
  at admission); #6844 (routes publish their contract rows when they go
  live).

### R-0022: Treat a handler's replies as undeclared unless its class declares them {#r-0022}

A handler promises a reply only through its class and return type: a single
handler returning `R` replies `R`, and one returning `()` is silent. A manual
handler may reply with any kind, any number of times, or not at all, and
declares nothing. A kind never declares its reply kind.

- **Why:** the same kind sent to different handlers may be answered
  differently, so a reply belongs to the handler.
- **Settled:** ADR-0109 (the return type is the reply contract); ADR-0231 §6
  (manual rows declare no reply kind); #6486 (a reply-typed manual handler
  declined).

### R-0023: Compose an absent capability as an honest stub {#r-0023}

A capability that may be missing on a chassis is composed there as a stub
that claims its mailbox and answers honestly: it absorbs fire-and-forget mail
or replies `Err` to a request. A declared dependency therefore always holds.
There is no optional dependency and no optional send.

- **Why:** an optional peer forces every sender to handle absence at every
  send site.
- **Settled:** ADR-0232 §6 (no optional peers; headless stubs claim the
  mailbox).

### R-0024: Keep a service that owns a shared resource as one actor {#r-0024}

An actor that manages a shared resource (host cores and memory, a daemon, a
content-keyed cache) stays one actor. When it seems to need a copy per unit,
the defect is that it holds one unit's data; the fix is to cut that binding,
for example by having the caller supply its storage on each request. Before
changing an actor's cardinality or placement, list the state it holds and
whose it is.

- **Why:** multiplying a resource manager splits one budget into idle shares
  and duplicates its caches.
- **Settled:** ADR-0240 D7 (one Bloomery workspace per engine over a typed
  storage source); #6828 (per-unit workspace budget shares declined).

### R-0025: Carry data between actors only as mail {#r-0025}

An actor's observations live in its own state, and anything another party
needs is a mail kind: a request answered by reply, or an observation mailed
to a registered sink. A shared `Arc<Mutex<..>>` handed in through params or
config and read from outside is not a channel.

- **Why:** a mutex side channel is invisible to tracing and settlement and is
  unordered against mail.
- **Settled:** #5965 (render observes dispatched kinds through the harness
  hook, not a mutex in params).

## Mechanisms and simplicity

### R-0026: Reuse the ctx and registry machinery for an engine check {#r-0026}

An engine check reads the facts an actor's ctx already reads, through the
same registry and ctx operations. It does not add a parallel table, index,
link-time inventory, or static to answer the question; a thin hook into the
existing owner is the most it adds.

- **Why:** a parallel structure answers a different question (what is linked
  rather than what is live) and is one more thing to keep in step.
- **Settled:** ADR-0241 §3 (the publication table is owned by the registry
  owner that already applies route contracts); #6858 (a contextual decode
  reads the mail registry that `resolve` and the ctx read).

### R-0027: Prefer one mechanism over two coupled ones {#r-0027}

When two mechanisms must agree to express one relationship, replace them with
one. Deepen an existing primitive before adding a parallel one; several
checks that guard the same transition collapse into one.

- **Why:** coupled mechanisms drift apart, and each consumer has to learn
  both.
- **Settled:** ADR-0241 §4 (one admission check replaces the load, boot,
  replace, and resolve checks); ADR-0240 D7 (the two-table funnel declined
  for a typed source on the request).

### R-0028: Add a record kind or event only for a condition that exists today {#r-0028}

Before adding a journal record kind, event, or fold, state the concrete
condition that needs it and confirm the condition can occur on `main`. A
defence against a condition that cannot occur yet is deferred.

- **Why:** each record kind multiplies traffic and the concepts every
  consumer must handle.
- **Settled:** ADR-0226 D9 (the reactor poison pill deferred).

### R-0029: Never drop a pending reply entry {#r-0029}

Reply tables and request-context tables never evict, overwrite, or refuse a
pending entry. They preallocate a fixed capacity, reuse freed slots
(generation-tagged where the table issues handles), grow when full, and warn
at each new high-water mark. Releasing a handle that provably can no longer
be answered is not a drop. No deadlines.

- **Why:** a dropped entry leaves a requester waiting silently, while growth
  makes a leak visible.
- **Settled:** #6412 (the reply table becomes a generation-tagged slab that
  grows); #6420 (the request-context table grows and never evicts).

### R-0030: Hold a deferred reply's chain open {#r-0030}

A handler that sends chain mail after it returns holds a `SettlementHold` on
the chain's root until its last such send. Completion is the exact settlement
signal, never a quiet window or a timeout.

- **Why:** a timing window is redundant for synchronous replies and cannot
  bridge a long asynchronous one.
- **Settled:** ADR-0080 §6 (the hold contract replaced the quiescence
  window, #1031).

### R-0031: Give each ordering concern one authority {#r-0031}

One mechanism decides an order. Draw order is the widget hierarchy's
depth-first flatten; there is no z-index or draw-layer field on render or
text kinds. Content that must cover other content sits later in the same
hierarchy.

- **Why:** a second ordering authority competes with the first, and every
  consumer has to reconcile the two.
- **Settled:** ADR-0117 §2 (draw order is structural); #5515 (draw layers
  declined).

### R-0032: Keep wasm and native actors symmetric {#r-0032}

A wasm actor behaves as a native actor does: the same naming, spawn,
dependencies, contract rows, lifecycle, replace, configuration, and
refusals. The only difference is where a handler body runs. A
target-specific design is accepted only when the difference is structural,
such as wasm having no threads; a first phase on one target defines the API
for both.

- **Why:** asymmetric behaviour makes the same library code act differently
  depending on where it is loaded.
- **Settled:** ADR-0241 §1 (a guest is a native actor whose handlers run
  wasm); ADR-0232 §5 (native capabilities use the same dependency model).

### R-0033: Build the missing primitive rather than downgrade the design {#r-0033}

When a design needs a primitive the engine lacks, build that primitive as its
own focused change. Do not ship a lesser shape (a singleton where the design
commits to instances) or fake the primitive with a workaround.

- **Why:** the decision commits the design, and a downgrade leaves the gap
  for every later consumer to route around.
- **Settled:** #2101 (boot-time instanced seeding built as the prerequisite
  for the sharded filesystem floor).

### R-0034: Change the structure when a fix needs a shim {#r-0034}

A fix that needs an invented marker, a hardcoded duplicate guarded by a drift
test, a relay, or a guard type is a sign the structure is wrong. Find the
shape that makes the shim unnecessary, such as a different crate, a bridged
actor, or a split of actor from driver logic.

- **Why:** a shim compiles and passes while papering over a structural
  mismatch that the next change hits again.
- **Settled:** #1379 (a marker helper for an unnameable lifecycle driver
  closed; the lifecycle became a bridged capability).

### R-0035: Do bring-up in a throwaway script component {#r-0035}

Putting a world into a starting state (loading assets, creating a camera,
seeding state) is done by a small component in the boot list that sends
ordinary typed mail from `wire`. A manifest field, config slot, or loader
option that sends mail at boot is not added.

- **Why:** a component already sends any mail its code chooses, typed and
  compile-checked, so a data-driven layer duplicates it in a weaker form.
- **Settled:** #6629 (boot-manifest init mail declined); #6626 (a throwaway
  demo component brings up the release demo); #6802 (a bootstrap script
  component builds the workspace environment).

## Rulings

Each ruling made after a rule lands is appended here, oldest first, and no
line is edited or removed:
`- <YYYY-MM-DD> · #NNNN or ADR-NNNN §N · <question in a few words> → <answer> · follows [R-NNNN](#r-nnnn)`.

- 2026-09-26 · #6842 · where the declaration-list boundary stops → at hand-written declaration impls; a type no `#[actor]` builds is follow-up #6870 · follows [R-0002](#r-0002)
- 2026-09-26 · #6842 (ADR-0231 §10) · visibility of a handled kind, dependency, or inline child → declared `pub`, and it may live in a private module · follows [R-0005](#r-0005)
- 2026-09-26 · #6865 · when the dependency row joins admission → with step 3 (#6866) · follows [R-0009](#r-0009)
- 2026-09-26 · #6865 · the `Publish` mail door and the module cache's move → deferred to step 5 · follows [R-0009](#r-0009)
- 2026-09-26 · #6865 · what a native publication records → its namespace only · follows [R-0026](#r-0026)
- 2026-09-26 · #6865 · how Bloomery bundles publish → each under its own per-digest namespace (the module hash) · follows [R-0021](#r-0021)
