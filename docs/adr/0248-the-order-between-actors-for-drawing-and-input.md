# ADR-0248: The Order Between Actors for Drawing and Input

- **Status:** Proposed
- **Date:** 2026-10-06

## Context

Two gaps met in one case: a debug overlay over a scene. The overlay must always be drawn over the scene, and while its console is open the keys must go to the console and to nothing else. Neither can be written today.

**Screen-space draws from two actors have no order.** The three overlay verbs (`aether.render.draw_textured_quads`, `draw_shapes`, `draw_screen_triangles`) each push one batch onto a single vector, `overlay_frame` in `RenderCapabilityState` (`crates/aether-render/src/runtime/mod.rs`), in the order the render capability received the mail, and that vector is recorded as the painter order. A lifecycle stage is broadcast to its subscribers as one burst that the scheduler may spread over several workers, so two actors that draw in the same stage reach the renderer in an order that can differ from frame to frame. The workaround found while scoping the overlay was to draw on `Present`, which is broadcast after the `Render` chain settles. That uses stage order to get draw order, and it stops working when a second actor does the same.

**Input goes to every subscriber.** Both window backends publish through one line, `ctx.fanout(self.subscribers.recipients::<K>(window), event)` (`crates/aether-window/src/runtime/desktop/mod.rs`, `crates/aether-window/src/runtime/synthetic/mod.rs`), and `recipients` is the whole subscriber set for that window (`WindowSubscribers` in `crates/aether-window/src/runtime/subscribers.rs`). Nothing lets one subscriber have an event. With a camera controller live (`crates/aether-kit/src/camera/controller/mod.rs` subscribes to `Key`, `KeyRelease`, `MouseButton`, `MouseButtonRelease`, `MouseMove` and `MouseWheel`), typing into a console also moves the camera, and a wheel over a scrolling panel also zooms the scene.

**Text is filed under the wrong actor.** `aether.text` turns a text draw into glyph quads and sends them to the renderer itself (`emit_draw` in `crates/aether-text/src/runtime/layout.rs` calls `ctx.send::<RenderCapability>`), so every glyph batch in the engine arrives as mail from `aether.text`, one hop behind the shapes it belongs with.

Prior decisions this one meets:

- **ADR-0117** made draw order inside one widget root structural (the root is the one render sender for its subtree, and its own composition order is the order) and left order between roots unbuilt under "Ordering escape hatch (deferred)". It rejects an absolute key. This ADR builds the part of that escape hatch that concerns order between actors.
- **ADR-0164 §4** states the publish rule: a window event is fanned out to `recipients(window, K::ID)`. This ADR changes that rule for key, text and pointer events.
- **ADR-0167** keeps subscriptions and cross-window policy with the manager, `aether.window`. The order is manager policy of the same kind.
- **ADR-0141** routes input between the regions of one editor shell by having the shell subscribe once and forward. It is discussed under Consequences.

Two scoped issues hold the code reading this ADR rests on: #7513 (draw order) and #7514 (input). Where their plans differ from the model below, this ADR is the decision: #7514's separate hold is folded into the order, and #7513's "an unplaced actor draws in the base" is replaced by a refusal.

The idea, in the owner's words:

> Why can't we have different types of topologies? Draw topology, input topology, etc.

and on the default:

> So could we say that actor toplogy for...lineage? Is the same as the other topologies. Unless explicitly declared otherwise?

## Terms

- **Order.** The one back-to-front sequence of actors that drawing and input both read. There is one per application (see Limits).
- **Member.** An actor that has a position in the order.
- **Peers.** Actors the actor tree does not order: the root actors among themselves, and the children of one parent among themselves.
- **Place.** A member's statement of where it stands among its peers.
- **Takes.** What input a member takes as part of its place: a pointer region, the keys, both, or neither.

"Focus" is not used for any of this. At this mailbox it already names operating-system window focus: `aether.window.focus` brings a window to the foreground and `aether.window.focus_changed` (`WindowFocus`) reports it. The new kinds use "place" and "takes", and the prose says a member "has the keys".

## Decision

### 1. Lineage is the topology

The order between actors follows the actor tree by default. A child is in front of its parent, for drawing and for input, and nothing is declared for that. A subtree moves as one: everything beneath a member lies between that member and the next peer in front of it.

Inside one actor nothing changes. An actor's own batches keep the order it sent them (same-recipient FIFO), so a widget root's composition order (ADR-0117) is still its own.

### 2. Where lineage is silent, an actor says

The tree has no order between two root actors and none between two children of one parent. There, and only there, an actor states its place: in front of one named peer, or in front of every peer that currently has a place.

```rust
// main: no order; both draw and race
ctx.send::<RenderCapability>(&DrawShapes { space, clip, shapes });

// plan: a root says where it stands, once, in `wire`
ctx.send::<WindowCapability>(&Place { over: Over::Members, takes: Takes::NOTHING });
ctx.send::<WindowCapability>(&Place { over: Over::Actor(scene), takes: Takes::NOTHING });
```

The kind names and field shapes are open question 1. What is decided is the vocabulary: a place is a relation to a peer or to the current members, never a value.

Who says it is not fully settled. The model as first agreed has the actor state its own place. The owner has since said that the order among roots "seems like something that an application would have to configure that the actor itself didn't declare", that it is inherited otherwise, and that it "only REALLY applies to roots. Sorta." Section 18 examines how much of the order the application's own authored lists can supply, and open questions 3 and 8 hold the choice. Under every option the statement above remains the way a root outside any authored list gets a place, and the way any member restates one.

A place is a position in a list of peers, so no cycle can be written: placing over a peer inserts the sender directly in front of it, and placing again moves the sender.

### 3. A root that draws and has no place is refused

A root that draws and has not stated its place has no position. Its screen-space draws are refused with an error that names the declaration. There is no default position and no guess. The same holds for an actor beneath such a root: the error names the root that has not spoken.

Existing drawers must place themselves. Found on `origin/main` at `4558b4175` with `git grep -n "DrawShapes\|DrawTexturedQuads\|DrawScreenTriangles\|DrawTextBatch" -- crates ':!crates/aether-render' ':!crates/aether-text'`:

- `crates/aether-widget/src/lib.rs`: `emit_layer`, the one sender for every widget root (three overlay verbs and `DrawTextBatch`).
- `crates/aether-kit/src/bundle/mod.rs`: the bundle tile's `DrawTexturedQuads` in `on_tick`.
- `crates/aether-test-fixtures-bundle/src/ui_widget.rs`: the fixture's `DrawShapes` in `on_tick`.

Tests that send overlay verbs with no actor sender are open question 7.

### 4. A member states what it takes

A member's place carries what it takes: a pointer region, the keys, both, or neither. A tooltip takes neither. A console takes the keys and the region it covers. A member that draws nothing can still take input: the camera controller is a member that takes the keys and the pointer over the whole window (see section 12).

Takes decide who is sent an event. Subscriptions still decide which kinds an actor is sent: the member that has the keys is sent `TextInput` only if it subscribed to `TextInput`, and a kind it did not subscribe to is sent to nobody.

### 5. Drawing reads the order back to front, input reads it front to back

The renderer lays the members' draws down from the back of the order to the front. An input event goes to the frontmost member that takes it: a key or text event to the frontmost member that takes the keys, a pointer event to the frontmost member whose region holds the point.

Nobody holds, grants or releases the keys. Which member has them is derived: the frontmost member that takes keys. Closing an actor returns the keys because the actor is no longer in the order.

| Moment | Order, back to front | Has the keys |
|---|---|---|
| Playing | scene, camera controller | camera controller |
| The console opens | scene, camera controller, console | console |
| A tooltip over the console, taking nothing | scene, camera controller, console, tooltip | console |
| The console closes | scene, camera controller | camera controller |

Events that are not key, text or pointer events keep the ADR-0164 §4 rule and go to every subscriber: `Modifiers`, `WindowSize`, `WindowFocus`, `WindowOpened`, `WindowClosed`, `WindowMenuActivated`.

### 6. Order is enforced where results are gathered

Mail sent in an order is not processed in that order, so no rule here sequences deliveries.

- **Drawing.** Every actor draws during the frame in any order. The renderer commits the frame when the engine-only `Frame` mail arrives (`on_frame` → `commit_scene`), and the driver sends `Frame` only after the frame's stages have settled (`run_frame_advance` then `send_render_and_drain` in `crates/aether-chassis-desktop/src/driver/mod.rs`). At that commit the renderer groups the batches by sender (`ctx.sender()`, stamped by the host) and sorts the groups by position.
- **Input.** Each event has one recipient, so there is nothing to order.

```rust
// main: one vector, receipt order
state.overlay_frame.push(OverlayBatch::shapes(mail));

// plan: filed under its sender; sorted once, at commit
state.overlay_frame.file(ctx.sender(), OverlayBatch::shapes(mail));
// commit_scene: groups in order, back to front; inside a group, arrival order
```

Two senders are compared by their canonical paths (`NativeCtx::actor_path` answers one for a proven reference). If one is an ancestor of the other, the descendant is in front. Otherwise the first step at which the paths differ names two peers, and the places stated among those peers decide.

### 7. The order is dynamic

Actors are born and close, a root or a sibling restates its place, and a member changes what it takes. The renderer reads the order once per frame, at commit, so a change shows at a frame boundary. Input reads the window's order when the event arrives, so for less than one frame a newly placed member can be sent input before it is drawn in front.

### 8. Two members that take keys, neither in front by statement

Two text boxes in separate actors both take keys. The user decides by clicking: the click goes to the member under the pointer and that member comes to the front. "The one in front has the keys" is the whole rule. How the member comes to the front is open question 6.

### 9. A press owns its release

The window remembers which member was sent each key-down and each drag start. The key's repeats and release, and the drag's moves, wheel and button release, go to that same member whatever the order is by then. A key held when the console opens does not stick, and a drag begun on the scene continues across the console.

This is a routing rule over the same order, not a second mechanism. A window that loses operating-system focus forgets its owners, because those releases never arrive (the rule `docs/guide/systems/input.md` already gives subscribers). An owner that closes takes its entries with it.

### 10. No number anywhere

No z-index, layer, band or priority value exists in a kind, a config or a declaration. Position is the tree plus relations between actors. ADR-0117 already rejects a number; this ADR builds the relation it left open.

### 11. Text is filed with its owner's shapes

`aether.text` sends its glyph quads with `NativeCtx::forward_to` (`crates/aether-substrate/src/actor/native/ctx/send.rs`), which keeps the inbound mail's sender as the forwarded mail's sender. The renderer then files each glyph batch under the actor that asked for the text. Atlas `CreateTexture` and `UpdateTexture` sends stay the text capability's own. This is part of the decision: without it every glyph belongs to `aether.text`, a root born at boot, and no position puts an actor's text with its shapes.

```rust
// main
ctx.send::<RenderCapability>(&draw);

// plan
ctx.forward_to(render, &draw);
```

### 12. A subscriber that is not a member

An actor that subscribes to keys and has no place can never be the frontmost member that takes them, so it is sent no key, text or pointer event. The camera controller draws nothing and is the first such actor. It becomes a member the way a drawer does: it states its place in `wire` and takes the keys and the whole window.

```rust
// main: crates/aether-kit/src/camera/controller/mod.rs, wire
ctx.subscribe::<WindowCapability, Key>();

// plan
ctx.send::<WindowCapability>(&Place { over: Over::Members, takes: Takes::keys_and(Pointer::Window) });
ctx.subscribe::<WindowCapability, Key>();
```

What the window tells a subscriber of a routed kind that has no place is open question 5.

### 13. A library root

A generic overlay cannot name the application's scene type, and it should not need a second file that says where it goes. Four ways a library root can get a place, and what exists for each:

- **It states "in front of the current members" itself.** New (the `Place` kind), and needs nothing from the composer. It is correct whenever the overlay is loaded after what it covers.
- **A path handed in its config.** The overlay's `Config` carries an optional path and the overlay places itself over it. Config exists today (`load_component` and `spawn` take `config`), and a boot manifest entry carries one, so the composer writes the relation where it already writes the component. Nothing new beyond `Place`.
- **The composer's authored order.** A boot manifest's components are loaded in order and each is awaited (`boot_standard`, `crates/aether-chassis/src/boot.rs`), and native roots are composed in the chassis's `with_actor` chain. Neither order is recorded anywhere after boot, and a component loaded later over MCP has no place in it. Reading it as the order would be new and is the "order of joining" this ADR does not want as a rule.
- **A parent places its child.** A parent holds a proven reference to a child it spawned (`NativeCtx::spawn_child`, `WasmCtx::spawn_inline_child` returning `InlineChild<C>`). No mail today lets one actor state another's place. New, and it only helps children, not roots.

Whichever is chosen, "in front of the current members" is available to a library root, so it never needs a second file. Whether the composer's authored order is read as the root order (section 18), and whether a parent may place its children, is open question 3.

### 14. Overriding the default, and instanced roots

**Child over parent is not overridden.** Rule 2 lets an actor speak only where the tree is silent. An actor that must stand somewhere other than in front of its parent is composed as a peer of whatever it must be ordered against (a root, or a sibling) and states its place there. A child that must float above everything (ADR-0117's tooltip or modal that escapes its tree slot) is the one case this leaves out, and is open question 4.

**Instanced roots.** Instances of one type (`NS:key`) are roots with no shared parent, so each is a peer of every root. Each instance stating "in front of the current members" in its own `wire` orders the instances by birth with no new mechanism, and any of them can restate. What that cannot say is a relation that holds for instances born later: "every nameplate is under the overlay". A place stated for a type, inherited by its instances, would say it. The registry's address index is already keyed by declared type (`AddressIndex` in `crates/aether-substrate/src/mail/registry/address.rs`), but nothing in the engine records an order among live instances. This is open question 2.

### 15. Where the order lives and how the renderer learns it

Facts from the code:

- A sender's canonical path encodes its ancestry, and any native actor can read it from the proven reference (`NativeCtx::actor_path`, answered from the route table's `RouteRecord::canonical_name`).
- The registry keeps no order among siblings and no birth order. `Children` is two maps from namespace to declared type, and a route record is a name and a lifecycle.
- Under rule 1 a child never contacts the window. The window therefore cannot publish a flattened list of members without learning of every birth.
- The window already monitors every subscriber (`Holder` and its `MonitorHandle` in `subscribers.rs`) and drops its rows on the `MonitorNotice`.

Decided: evaluation is central. Actors declare and never forward. The window routes input from the order and the renderer sorts by it at commit. Neither holds a flattened list: both resolve ancestry from the sender's path and consult only the declared relations among peers (section 6).

Which actor owns the relations among peers is open question 8, between the window, the host registry, and the renderer. Section 18 examines the registry option against the code.

### 16. Absent members, skipped frames, refused draws

- **The order does not depend on presence.** A member that submits nothing in a frame is an empty group. Its position stands and nothing shifts.
- **Draws are immediate mode.** A member that skips a frame vanishes for that frame, as it does today (ADR-0105). Under `replay_cache_when_idle` the renderer replays the last committed list when a whole frame submitted nothing (`commit_or_replay`); the replay keeps the groups and is sorted by the order current at that commit, so a place stated while the scene is idle still shows.
- **A dropped advance.** A `LifecycleAdvance` that arrives while one is in flight is warn-dropped and no stage is skipped (`docs/guide/systems/lifecycle.md`). No `Frame` is sent for it, so no commit happens and no partial set of draws is sorted. Batches wait in the accumulator for the next commit, as they do today.
- **A refused draw** is missing from the frame and nothing else changes. The other members' groups are sorted as if the refused sender had sent nothing. The refusal is an error naming the sender and the declaration, reported once per sender until its state changes, never once per frame.
- **Retained draws** (ADR-0246) are world-space sets a render program draws. They are ordered by depth and by their program, and this ADR does not touch them.

### 17. Republish

Membership, place and takes belong to the mailbox and stand across a republish, as subscription rows do. A component carries its own matching state (whether its console is open) through `on_dehydrate` / `on_rehydrate` like any other state. There is no special rule.

### 18. The order kept beside the lineage, examined

The owner called keeping the order in the host registry beside the lineage "smart if there was a cheap/easy way to get the current topology and sort things based upon that", and named the hard part: sorting actors whose order is ambiguous, with no way to make ambiguity a compile-time error. This section says what the code supports for that today and what would be new. Nothing in it is decided; it feeds open questions 3 and 8.

**A cheap read.** The route table is `routes: View<FxHashMap<MailboxId, RouteRecord>>` (`crates/aether-substrate/src/mail/registry/mailbox/mod.rs`), and a `View` is an `ArcSwap` of a published value (`crates/aether-substrate/src/mail/view/mod.rs`). A read is one atomic load and one hash lookup and takes no lock: `Registry::actor_path` is `self.routes.load().entry_for(&actor.id())`. Writes go through the registry owner's batches (`crates/aether-substrate/src/mail/registry/owner.rs`), on one thread.

What a comparable position needs and the registry does not have: any instance-level record of which live actors are children of which, and any order among them. A route record is a canonical name and a lifecycle, and `Children` in `address.rs` is type-level (which namespaces may sit beneath which).

The shape that fits: the owner keeps, per parent and for the roots, the list of its live children in order, appends at the batch that makes a route `Live`, and removes at close. A member's position is its chain of indices from the root down, compared left to right, with a shorter chain that is a prefix of a longer one behind it (the parent is behind its child). It is derived, never authored, and never leaves the registry as a value an actor could write. The renderer asks for it once per sender group at commit, and a frame has a few groups. Cost: one append or one removal in a batch the owner already runs for that birth or close; a read of depth-many lookups per group; no new lock. Not measured: what republishing the view costs per change, and what a removal costs beneath a parent with tens of thousands of inline children. Recomputing one flat rank for every member on each change is the alternative and costs a walk of the whole tree per birth, which the per-parent form avoids.

With an order kept this way every pair of members is comparable, so the ambiguity that cannot be a compile-time error does not arise for anything born through an authored list. It remains only for the case in the last paragraph of this section.

**Children: the `spawns(..)` list.** `#[actor(spawns(A, B))]` lists a guest spawner's child types in authored order. The macro keeps that order: it emits `type Spawns` as a type-level list and one impl per entry with its index (`crates/aether-actor-derive/src/reply_markers.rs`, `wasm_expand.rs`). So for a guest parent the order between child types is known at compile time and could be the sibling order between types, with instances of one type ordered by birth. Three things are new. The list is wasm-only: a native actor that writes `spawns(..)` is a compile error ("native actors spawn through `spawn_child`"), and a native child declares the edge from its own side with `child_of(..)`, collected at link time as `ChildEntry` facts that the code calls "anonymous facts rather than a named topology" and that have no order. Whether a module's manifest carries the `spawns` order to the host was not checked. And a list whose order means nothing today would start to mean something: reordering the attribute would change what is drawn in front.

**Roots: the application's authored lists.** The lists exist. A chassis composes its native roots in a `with_actor::<..>` chain (`crates/aether-chassis-desktop/src/chassis.rs`). A boot manifest's components are "auto-load on boot, in order", each awaited (`BootConfig` and `boot_standard`, `crates/aether-chassis/src/boot.rs`). `spawn_substrate` takes a `components` list. None of them is kept as an order after boot. Because each entry is awaited before the next, the order in which roots become `Live` at boot is the authored order, so an owner that appends each root as it becomes `Live` would record the application's list with no actor declaring anything. That is the reading under which the application configures the root order. Not checked: that `spawn_substrate`'s `components` reach the same awaited loader.

**Roots: a relation declared on the type.** The other form is that a root type says it itself, in its own attribute, and the engine sorts the roots from what the types say. The owner's words: "You could even have it be by declaration with some sort of structure which can hand sort itself? Declare itself?"

```rust
// main
#[actor(instanced, root, depends(WindowCapability, LifecycleCapability))]

// plan: a root that can name the other type
#[actor(instanced, root, over(Scene), depends(WindowCapability, LifecycleCapability))]

// plan: a root that can name nobody
#[actor(singleton, root, front, depends(RenderCapability, TextCapability))]
```

`over(..)` takes types the way `depends(..)`, `spawns(..)` and `child_of(..)` already do, so the name is checked by the compiler and no new syntax is needed. `front` is a bare word like `root`. The spellings are not settled. What exists: the derive's type-list arguments, and the publish path that reads every module's declared facts into the address index before anything spawns (`AddressIndex::from_publications`). What is new: the two arguments, carrying them in the native inventory and the guest manifest, and the sort.

The engine would sort the root types at boot and at each publish: a type is in front of every type it names, a `front` type is in front of every type that is not `front`, and instances of one type are ordered by birth. It refuses a cycle, and it refuses two root types that both carry a word and that nothing orders (two `front` roots, or two roots over the same type). Both refusals come at publish, before `init`, with both type names in the error. A root type that carries no word has said it does not draw or take input, so its first draw is the refused case of rule 3.

This form answers three things the list does not. A relation between types holds for instances born later, which is open question 2. A root loaded mid-session carries its relation in its module, so it is not "outside" anything. And the error for a missing or contradictory order moves from first draw to publish.

**The two forms compared.**

| | Authored composition list | Relation on the type |
|---|---|---|
| Who has the knowledge | The application, which sees every root | The type's author, who knows what it is drawn over when it can name it |
| What is checked, and when | Nothing to check: a list is a total order | The name at compile time; cycles and unordered pairs at publish |
| A library root that names nobody | Fits: the application gives it a line | `front`; two such roots are refused until something orders them |
| A root loaded mid-session | Has no line; needs a statement of its own | Carries its relation |
| Changing the order | Edit the list | Edit a type, or restate by mail at run time |

The forms fail in opposite places. A type cannot order itself against a type it cannot name, and that is exactly where the application has the knowledge. A list cannot cover a root that arrives after boot, and that is exactly where the type's own relation travels with it. So the lean is that both exist with different jobs: the relations on types are constraints the engine checks, and the application's list orders what the types leave unordered (two `front` roots). A list that contradicts a relation is refused at boot, naming both. If only one is built first, the relation on the type is the one, because it brings the publish-time error and covers later loads; two `front` roots then stay refused until the list exists.

**A root outside any authored list.** A component loaded mid-session by mail or over MCP, a native actor loaded later, or one arriving from another journal has no line in any list. Appending it at its birth would be order by arrival, which two loads sent together can race. So it must be said: the root says "in front of the current members" or "over X" itself, or its loader says it in the load (a place argument on `aether.component.spawn` and `load_component`, which is new). A root with neither is the refused case of rule 3. When the error can be raised:

- **Not at compile time.** The modules are built separately and the set of roots is decided by whoever composes them.
- **At birth, for a place the loader states.** The component host already refuses a birth before `init` when a declared dependency is not live (`crates/aether-component/src/component/runtime/dependencies.rs`). A loader's place naming an actor that is not live can be refused at the same moment.
- **At birth, for a missing place,** only if the type says it needs one. Nothing declares today that an actor draws or takes input. A word in `#[actor(..)]` would let a load with no place be refused before `init`. That is new.
- **At first draw** otherwise. Without such a declaration the engine learns that an actor draws when its first batch reaches the renderer, and that is the earliest the refusal can come.

How actors from another journal are born was not read.

## Consequences

- **A cross-actor overlay can be written.** The debug overlay places itself in front, takes the keys while its console is open, and neither the scene nor the camera controller changes when it is loaded.
- **Keyboard focus needs no protocol.** There is no take, grant, release or "lost input" event. An author cannot forget to release, because there is nothing to release.
- **Every drawer and every input consumer places itself.** `aether-widget`, the kit bundle tile, the camera controller and the fixtures gain a `Place` send. A component that forgets is told by an error on its first draw rather than drawing in an order that varies.
- **ADR-0164 §4 is amended.** Key, text and pointer events have one recipient. The other window events keep the fan-out. The amendment line is written on ADR-0164 when this is implemented.
- **ADR-0117 is completed in part.** Order between roots, and between an actor and a child that draws for itself, is built. Its open question (key versus edges, and who owns order between roots) is answered: edges, owned where section 15 and open question 8 settle. A widget subtree still reaches the renderer through one sender, so nothing inside a root changes.
- **ADR-0141 stands and is now the exception.** The editor shell subscribes once and forwards to its regions. Under this ADR the shell is one member and its forwarding is its own business. Re-expressing regions as members, which would remove the shell's `Routing` table, is follow-on work and is not decided here.
- **ADR-0105 is amended.** Glyph quads reach the renderer with the asking actor as sender.
- **Guides.** `rendering.md` ("in submission order"), `window.md`, `input.md`, `text.md` and `widgets.md` ("one hop behind") change with the implementation.

### Limits

- **No bubbling.** A parent cannot decide per event after its child looked. That is a request and a reply per level, and is left for when something needs it. A console that ignores F5 cannot pass it down.
- **"Always in front of whatever joins later" has no number-free form.** A later actor that places itself in front of the current members goes over the overlay. Accepted. A place stated for a type (open question 2) would relax this for instances of a named type and for nothing else.
- **One order per application.** The render scene is application-scoped: `on_frame` commits one scene and records it for every dirty window, and no draw kind names a window. A member's place and what it takes therefore apply at every window, and a pointer region is read in each window's pixels. An order per window first needs draws that name a window.
- **Text and shapes inside one actor.** A plate that must cover text the same actor sent earlier is an order between two batches of one group that reach the renderer through two recipients. Position does not solve it and it is out of scope. The two candidate shapes are: text answers with quads and the caller draws them in its own order; or text becomes an overlay arm the renderer lays out itself. Until one is built, a thing that must cover its owner's text is its own member (a child, which is in front of its parent's whole group).
- **IME.** `ImePreedit` and committed `TextInput` are routed with the keys. Whether a member says it wants text, and what happens to a composition in progress when the member that has the keys changes, is not decided here.
- **Hover.** A member that highlights under the cursor keeps its highlight when the cursor enters a region in front of it, because moves stop arriving and nothing tells it.

## Open questions

Each has options and a lean. None is decided.

**1. The kinds, and whether place and takes are one declaration.**

```rust
// (a) one kind
Place { over: Over, takes: Takes }

// (b) two kinds
Place { over: Over }           // peers only: roots and siblings
Take { keys: bool, pointer: Option<Pointer> }   // any member
```

Under rule 1 an only child has nothing to say about place and may still take input, so (a) needs an "as lineage has it" value for `over`. (b) has no such value, and changing what a member takes (the console opening) does not restate its place. Lean: (b). Names lean `aether.window.place` and `aether.window.take`; neither uses "focus" or "hold".

**2. A place stated for a type.** (a) Instances only: each instance places itself. (b) `over` may name a namespace, and the relation holds for every instance of that type, present and future; instances of one type are ordered among themselves by their own statements. (b) is the only number-free way to say "under the overlay, whenever born". It needs an order among live instances that nothing records today. A relation declared on the root type (question 3, option (d)) is (b) written at compile time. Lean: (a) if the order is stated by mail; settled by option (d) if that is built.

**3. Who states the order among peers.**

- (a) Each actor states its own place, roots and siblings alike. The model as first agreed.
- (b) The application's authored lists are the root order and a guest parent's `spawns(..)` list is the order between its child types, with instances of one type by birth (section 18). An actor states a place only when it is a root outside every list, or to restate.
- (c) A parent may place a child it holds a reference to.

- (d) A root type declares its relation in its `#[actor(..)]` attribute, `over(Type)` or `front`, and the engine sorts the root types at boot and at each publish (section 18).

(a) makes every drawer and input consumer send a `Place` and leaves two silent siblings ambiguous. (b) matches the owner's later words: the application configures the roots, everything else is inherited, and almost nothing declares. Its costs are the new machinery section 18 lists, a meaning for the order of `spawns(..)`, and no authored list on a native parent. (c) is small and helps only children. (d) is checked earlier than any other form and cannot order two roots that cannot name each other. Lean: (d) and (b) together, as section 18 compares them: relations on types are checked constraints, the application's list orders what they leave unordered, and children follow `spawns(..)` and birth. The `Place` mail of (a) remains for restating at run time (a clicked member coming to the front) and is no longer how a root first gets its place. (c) only if a native parent with several drawing child types appears before `spawns` has a native form.

**4. A child that must escape its parent's slot.** (a) Not expressible: compose it as a root. (b) A child may place itself among the roots. (b) is ADR-0117's "lift a flagged subtree to a higher root" and breaks "a subtree moves as one". Lean: (a).

**5. A subscriber of a routed kind that has no place.** (a) The subscribe is answered with an error naming the declaration. (b) The subscription stands, the actor is sent nothing, and the window reports it once. (c) It is sent every event as an observer. (c) restores the bug for any consumer that forgets. (a) depends on the place arriving before the subscribe. Lean: (b).

**6. How a clicked member comes to the front.** (a) The window moves any member that is sent a press. (b) The window moves it only when it takes keys. (c) The member restates its place when it is pressed. Under (a) a click on a scene that takes the pointer would bring the scene in front of the overlay and draw over it. Lean: (b), and the move is among its peers only.

**7. Draws with no actor sender, and silent siblings.** A harness root push and an MCP session may send an overlay verb with no member behind it; whether `ctx.sender()` is `None` for both was not checked. Options: refuse them as rule 3 refuses a root; or file them in one group behind every member. Two children of one parent that both draw and have stated nothing are the same question one level down: refuse both when the commit cannot compare them, or require a place of every child that has a sibling. Under option (b) of question 3 the sibling half does not arise. Lean: senderless draws form one group at the back, because a session cannot be told and cannot close; under option (a) of question 3, incomparable siblings are refused at commit with the same error.

**8. Who owns the declared relations.**

- (a) The window owns them and publishes them on change through its subscriber table; the renderer subscribes (`RenderCapability` gains `depends(WindowCapability)`) and resolves ancestry from each sender's path. Departure rides the monitor the window already takes. One new published kind, and it is machinery that exists.
- (b) The host registry, which owns lineage and sees every close, also keeps the relations among peers; the renderer and the window read it when they resolve a sender, with no mail edge between them. Nothing is published and no monitor is needed. It puts a drawing and input concept in core machinery, and takes still live in the window.
- (c) The renderer owns them. Input then reads the renderer's state, and pointer order and draw order sit in the actor that has no input.

A variant of (a) carries the relations on the engine-only `Frame` mail instead of a published kind. Three drivers build `Frame` (`crates/aether-chassis-desktop/src/driver/mod.rs`, `crates/aether-chassis-harness/src/pump.rs`, `crates/aether-harness-substrate-capture/src/ext.rs`) and two of them do not go through the window manager.

Section 18 found (b) cheap to read: a lock-free load the renderer and the window already perform to name a sender, and one append or removal in a batch the owner already runs. What (b) adds to core machinery is an instance-level child list the registry has never kept, on the birth and close path of every actor, whether or not it draws; a change under that rule has to name the cost, and the cost of republishing the view per change is not measured. What it removes is the ambiguity: an order kept at birth is total, where declared relations leave every pair that nobody related incomparable.

Lean: (b) for the order, with takes and press ownership in the window, provided the measurement holds. (a) with the published kind is the fallback and is the smaller change to today's code. A restated place under (b) is a registry effect the owner applies, so "comes to the front" (question 6) moves one entry in one parent's list.

**9. The pointer region.** A rectangle in the physical window pixels the pointer events carry, or the whole window. A drawn shape is not a rectangle. Lean: rectangle and whole-window only; a list of rectangles when a case needs it.

## Alternatives considered

- **Draw on `Present`.** Uses stage order as draw order and stops working when a second actor does the same.
- **A new lifecycle stage for overlays.** A second stage to carry what a position carries, with the same failure at the third actor.
- **A number, band or enum on the draw kinds.** ADR-0117 already rejects an absolute key: every author must agree on one scale.
- **Order from `#[actor(depends(..))]`.** An edge that means "must be live first" would also mean "draws under". The camera depends on the lifecycle capability and every widget root on render and text, and none of those is a paint relation.
- **Routing every draw through an owner actor.** Puts every draw one hop behind, which is the text problem for everything.
- **A separate hold beside the order.** The first scope of #7514 had one: a stack of holds ordered by the time each was taken, the top in force. It was a second order that could disagree with the draw order, it needed a release an author could forget across a republish, and it could not express a panel that takes clicks without the keys. Once members state what they take, the frontmost member that takes keys is the hold, and nothing is left for a second mechanism to do.
- **Pointer routing with its own order.** A click could land on something drawn underneath what the user sees.
- **Decentralized routing.** Each actor passes events to its children and may refuse per event. It costs a hop per level, a parent that traps or forgets silences its subtree with no message, and every project writes forwarding again. It is what would give bubbling, and is left for when something needs that.
- **An unplaced actor draws in a base under every member.** #7513's lean. It breaks nothing today and leaves a component that forgot to place itself wrong with no message.
- **Order of arrival.** The smallest mechanism, and for loads that are not sequenced it makes order a property of timing: two roots loaded together get a different order on different runs. An authored list whose entries are awaited one at a time is a different thing, because its order is written down by the application (section 18).
