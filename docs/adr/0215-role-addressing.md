# ADR-0215: Role addressing

- **Status:** Provisional
- **Date:** 2026-09-08

## Context

Addressing resolves a *type* to a position. `ctx.peer::<R>()` folds
`aether.embedded:<R::NAMESPACE>` onto the mailbox the runtime retains as the
caller's logical parent (ADR-0099 §5 and its 2026-08-05 amendment,
`PeerCtxExt::peer` in `crates/aether-component/src/component/route.rs`). The
fold is pure and client-side: no registry read, no round trip, no wire field.
Everything about *which* mailbox a typed send reaches comes from
`R::NAMESPACE`, a const the recipient type owns.

That is exactly right for "the camera", and it has no answer for "the thing
currently filling this position". There is no notion of a **role** an actor
occupies. When a new implementation has to take an old one's place in a mail
graph that other actors already address, the only lever is the load `name`, so
the substitute gets loaded under the incumbent's namespace string.

Lunaris is the worked example. A widget-based studio replaced the HUD sheet.
The planner still pushes to `lunaris.hud`, so the studio is loaded under the
name `lunaris.hud`, and its own peers reach it with
`peer_named::<StudioPanel>(STUDIO_COMPONENT)` where
`const STUDIO_COMPONENT: &str = "lunaris.hud"`. The consequences compound:

- `ctx.peer::<StudioPanel>()` folds `lunaris.studio`, which nothing registers.
  It compiles clean and warn-drops at delivery. The correct-looking call is the
  broken one.
- A consumer crate now declares a const holding a *different* actor's
  `NAMESPACE`, the second naming authority iamacoffeepot/aether#5720 is
  about, and one its proposed lint would flag as the violation rather than
  as the workaround.
- The tests inherit the hack, so the address the scenarios prove is the address
  the workaround produces, not the address the design intends.

The escape hatch is already half of the mechanism. `peer_named::<R>(name)`
calls `__actor_with_namespace::<R>(name)`: it substitutes a runtime string for
`R::NAMESPACE` in the fold while keeping `R` for the handler and kind checks.
Sender-side, "address this type at a name that is not its own" is a supported
operation. What is missing is that the name is a bare `&str` at the call site,
so nothing checks that the two ends agree on it, nothing declares that the name
is a shared position rather than an actor's private identity, and nothing
verifies that whatever registers there can handle the mail the senders send.

Constraints carried in:

- **ADR-0099.** `MailboxId` is the fold over a lineage of `ActorId`s, resolved
  client-side with no lookup. A role must not add one. One claimant per
  position is enforced at registration, where a publish under a taken name
  returns `NameConflict`
  (`crates/aether-substrate/src/mail/registry/mailbox/register.rs`), never at
  compile time: two types can declare the same `NAMESPACE` and compile.
- **ADR-0166.** A namespace is declared once, by the actor that owns it.
  Relationships and abbreviations never repeat a namespace string. The `://`
  abbreviation grammar is a string-boundary spelling that expands to a
  canonical lineage before hashing; it is not a routing language for Rust
  callers, and no new URI syntax may be invented.
- **ADR-0038.** `replace_component` swaps a module behind a stable mailbox id.
  Substitution *of the same load* is already solved; substitution *by a
  different actor type* is not.
- **ADR-0136.** A route key can hold a member set that instances opt into
  together. That mechanism is route-level and load-spreading, and its
  precedent worth carrying is the opt-in: sharing a key is something both ends
  declare, never something a second registration silently gets.

## Decision

A **role** is a first-class identity that no implementation owns, addressed by
type on the sending side and claimed by declaration on the receiving side.

### 1. A role is a declared identity with no code

A role is a ZST with a `NAMESPACE` and a required handler set. It has no
`Actor` impl, no state, and no handlers of its own:

```rust
#[actor(role)]
pub struct BuildScreen;
```

It lives where the peers already meet: the shared kind crate (ADR-0066), which
both the incumbent and the substitute depend on. The role's namespace is
declared exactly once, on the role, so neither implementation carries a copy
of the other's name and iamacoffeepot/aether#5720's rule holds without an
allow-list entry.

A role declares placement the way an actor identity does (ADR-0166 §1), so it
folds at a position rather than floating.

### 2. Senders address the role by type

```rust
ctx.role::<BuildScreen>().send(&PushPlan { .. });
```

`ctx.role::<Role>()` folds `aether.embedded:<Role::NAMESPACE>` onto the
caller's retained logical parent, byte for byte the operation
`ctx.peer::<R>()` performs with `R::NAMESPACE`. This adds no resolution path,
no registry read, and no wire field. It is `peer_named` with the `&str`
replaced by a type both ends read from a single owner.

The kind bound is the role's. `send::<K>` compile-checks `Role: HandlesKind<K>`
against the required set the role declares, not against whichever
implementation is claiming it, so a sender is typed against the contract rather
than against an incumbent it should not know about.

### 3. An implementation declares that it claims a role

```rust
#[actor(claims(BuildScreen))]
impl WasmActor for StudioPanel {
    const NAMESPACE: &'static str = "lunaris.studio";
    ...
}
```

The claim is a compile-time obligation: `claims(Role)` fails to build unless
the claimant handles every kind the role requires. That is the check the
`&str` workaround cannot express, and it is the reason a role is worth more
than a shared const.

At load, the claim publishes a routing alias from the role's folded position to
the claimant's own `MailboxId`. One live mailbox, one handler, one identity;
the role position is an additional inbound address, not a second actor. The
alias is subject to the existing one-claimant-per-position rule: a second claim
of the same role under the same parent is a load error, the same `NameConflict`
a duplicate load name already produces.

### 4. The claimant keeps its own identity

`NAMESPACE`, `LoadResult.name`, the per-actor log ring, the cost table,
subscriptions, and `ReplyTo` all stay the claimant's own. `StudioPanel` is
loaded as `lunaris.studio`, is logged and introspected as `lunaris.studio`, and
is reachable by `ctx.peer::<StudioPanel>()` as well. Claiming a role adds a
door; it does not rename the house. `describe_component` reports the roles an
instance claims.

### 5. A role resolves under the caller's parent, like every other address

There is no global role table and no "whichever actor claims it anywhere". A
role folds beneath the caller's retained parent exactly as a peer does, so two
component hosts can each have their own claimant of the same role without
either being ambiguous, and moving a caller under a nested host moves its role
route the same way it moves its peer routes. Resolution stays a pure fold;
liveness stays a delivery-time fact, as it is for every static address today
(ADR-0099 §5).

### 6. Migration lifts a name rather than moving an address

An incumbent whose name is already a de facto role is migrated by *lifting*
that name out of the incumbent and onto a role identity:
`lunaris.hud` becomes `BuildScreen::NAMESPACE`, the old HUD gains a private
namespace of its own and `claims(BuildScreen)`, and the studio claims the same
role. The string that senders resolve to does not change, so no live address
moves, no registered name changes, and the swap is which implementation holds
the claim.

## Consequences

### Positive

- The broken-looking call and the working call converge.
  `ctx.peer::<StudioPanel>()` reaches the studio because the studio is loaded
  as `lunaris.studio`, and `ctx.role::<BuildScreen>()` reaches whatever fills
  the position. Neither is a warn-drop.
- The contract between the position's senders and its occupant becomes a
  compile error when it breaks, instead of a silent drop at delivery.
- No consumer declares another actor's `NAMESPACE`. The
  iamacoffeepot/aether#5720 lint becomes satisfiable for exactly the cases
  that motivated the workaround.
- No new resolution mechanism. Role resolution is the ADR-0099 fold over a
  namespace a type owns, so the no-lookup invariant, the wire format, and the
  external address grammar are all untouched.
- A substitute is swappable without a module swap. Where ADR-0038's
  `replace_component` requires the replacement to be the same load, a role
  lets a different actor type in a different module take the position.

### Negative

- A third identity to name. A role is a type someone must place in a crate both
  sides depend on, and getting that placement wrong reintroduces a dependency
  edge the split was meant to avoid.
- The registry gains an alias concept. An id can now resolve to a mailbox whose
  primary name is different, which every reverse-mapping surface (inventory,
  `actor_logs` addressing, trace rendering) has to render honestly rather than
  as two actors.
- The macro gains two forms (`role`, `claims(...)`) and the compile-time proof
  that a claimant covers a role's required kinds, which is real macro work.
- One more way to spell an address. `peer`, `peer_named`, `loaded`,
  `loaded_default`, and now `role` all reach a component, and the guide has to
  say plainly when each is right.

### Neutral

- A role is permission to occupy a position, not ownership, supervision, or
  liveness, the same qualification ADR-0166 §1 puts on `ChildOf<P>`.
- Nothing forces a position to be a role. A component with one implementation
  and no substitution story keeps addressing its peers by type, unchanged.

### Follow-on

- The interim warning the issue names is independent of this decision and worth
  landing regardless: `load_component` warns when the load `name` equals a
  *different* declared `NAMESPACE` in the same module. That is the exact
  signature of the workaround this ADR replaces, and it costs one comparison
  against metadata the module already carries.
- Migrating the lunaris HUD and studio, and the scenarios that inherited the
  workaround, is a separate change against this record.
- The guide's addressing section
  (`docs/guide/foundations/actor-model.md`, "Names and addressing") needs the
  role verb beside `peer` and `peer_named` once this lands.

## What this ADR does not decide

- **Native claimants.** Whether a chassis capability can claim a role, and what
  a role means at the root where `MailboxId == ActorId`. The forcing cases are
  all embedded components.
- **Claiming from the outside.** Whether a role can be claimed at load time by
  a `role:` field on `load_component` or a boot-manifest entry, rather than
  declared in the claimant's source. Declaration is decided; a load-time
  override is not, and adding one later does not disturb §2.
- **More than one claimant.** A role has exactly one claimant per position
  here. Whether a role can ever be a fan-in set over live members, the shape
  ADR-0136 gives HTTP routes and iamacoffeepot/aether#5727 proposes for
  `replicas`, is left open; the opt-in-on-both-ends property is preserved so
  that door stays reachable.
- **Failover.** Nothing re-claims a role when its claimant drops. A role is an
  address, not a supervision relationship.
- **Claims across a swap.** Whether `replace_component` carries a claim across
  the swap or requires the replacement to declare it, and what a drain window
  means for mail already addressed to the role position.
- **External spelling.** Whether a role namespace anchors or appears in the
  ADR-0166 §5 abbreviation index, and what `LoadResult` reports about claims.
  No new URI grammar is introduced here and none may be inferred from this
  record.

## Alternatives considered

- **Do nothing: load the substitute under the incumbent's namespace.** The
  status quo, and the reason this record exists. It works, in that the mail
  arrives, and it costs the substitute's own type-addressed identity: the
  correct-looking `ctx.peer::<Substitute>()` becomes a compile-clean warn-drop,
  a consumer crate has to declare a const holding the incumbent's name, and the
  arrangement is invisible to every tool because the registry sees an ordinary
  load under an ordinary name. It also does not scale past one substitution:
  the next implementation inherits a namespace two owners deep.
- **Give the substitute the incumbent's `NAMESPACE` literally.** Two types
  declaring one namespace compiles and collides at registration, so the
  incumbent and the substitute can never be loaded together, not even for a
  side-by-side migration. It breaks ADR-0166's single-owner rule outright and
  discards the substitute's identity in logs, cost tables, and introspection.
- **A runtime role registry consulted at send.** The literal reading of "the
  substrate resolves it to whichever actor claims it". Rejected: it puts a
  lookup on the send path, which is the invariant ADR-0029 and ADR-0099 exist
  to protect, and it would have to be reachable from guest code across the FFI
  boundary. Aliasing at registration buys the same behavior with the fold
  unchanged.
- **Reuse `replace_component` (ADR-0038).** It is the existing substitution
  mechanism and it does not fit: it swaps a module behind one mailbox and
  requires the replacement to be a drop-in for the same load, so a different
  actor type from a different module cannot use it. The loaded name also stays
  the incumbent's, which leaves the bare-type miss exactly where it is.
- **Reuse ADR-0136 shared target sets, or iamacoffeepot/aether#5727's
  base-name claim for `replicas`.** Both are many-actors-one-address, but
  for spreading load over
  interchangeable instances of the *same* implementation, selected round-robin.
  A role has one claimant and the point is that the implementations differ.
  Borrowing the member-set machinery would make "which implementation answered"
  nondeterministic, which is the opposite of the guarantee wanted here.
- **An alias at the ADR-0166 §5 string boundary.** Expanding `lunaris.hud://`
  to the studio's canonical lineage. Rejected on two counts: abbreviations are
  a string-boundary spelling that is never registered and never the identity,
  and the whole problem is in typed Rust callers, who by that ADR's own
  constraint must not gain a string routing language.
- **A forwarder actor registered at the incumbent's name.** A shim that relays
  to the substitute. It works with no new mechanism and costs a mailbox, a hop,
  and a re-declaration of the whole handler set per role, and it distorts
  `ReplyTo` and settlement so every traced chain gains a node that is not part
  of the design.
- **A `role = "..."` string on the claimant.** The sketch in
  iamacoffeepot/aether#5728: an `#[actor(role = "lunaris.build_screen")]`
  attribute beside `NAMESPACE`, with
  senders naming the role. It is the smallest change and it keeps the role a
  string, which is the part that fails. Nothing verifies that the claimant
  handles what the senders send, the sending side is back to a literal or a
  const, and the role string has no owner, so two claimants of the same role
  each spell it independently and a typo is a warn-drop. Making the role a type
  costs one declaration and buys the single owner and the compile-time
  obligation.
- **Keep `peer_named` and address the target by a const it owns.** The minimal
  fix already in the tree (`loaded_default::<R>()`,
  iamacoffeepot/aether#5720). It removes the duplicated literal and nothing
  else: the name is still the incumbent's
  private namespace, still the substitute's registered name, and still
  unchecked against what the senders send. It is right on its own terms and it
  does not answer this question.

## Related

- ADR-0099 (Actor identity and addressing). Role resolution is its fold, over a
  namespace a role type owns rather than an actor type.
- ADR-0166 (Typed actor lineage and abbreviated external addresses). Supplies
  the single-owner namespace rule a role satisfies by being its own owner, and
  the placement declarations a role reuses.
- ADR-0038 (Actor-per-component dispatch, superseding ADR-0022). Same-load
  substitution behind a stable mailbox; a role is the different-type case.
- ADR-0096 (Multi-actor wasm modules). `export` selects which exported type an
  instance becomes; a role selects which loaded instance a position resolves
  to. The two compose and neither replaces the other.
- ADR-0136 (HTTP route target sets). The opt-in-on-both-ends precedent, and the
  shape a multi-claimant role would take if it is ever wanted.
- ADR-0066. Components and their peers share the kind crate: where a role
  identity belongs.
- iamacoffeepot/aether#5720. Hand-declared `NAMESPACE` consts, the symptom
  this addressing gap produces.
- iamacoffeepot/aether#5727. Replicas and the base name, the adjacent
  many-actors-one-address question this record deliberately leaves open.
