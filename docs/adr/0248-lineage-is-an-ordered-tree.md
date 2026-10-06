# ADR-0248: Lineage Is an Ordered Tree

- **Status:** Proposed
- **Date:** 2026-10-06

## Context

Two gaps met in one case: a debug overlay over a scene. The overlay must always be drawn over the scene, a click on the overlay must not also reach the scene, and while its console is open the keys must go to the console and to nothing else. None of the three can be written today.

**Screen-space draws from two actors have no order.** The three overlay verbs (`aether.render.draw_textured_quads`, `draw_shapes`, `draw_screen_triangles`) each push one batch onto a single vector, `overlay_frame` in `RenderCapabilityState` (`crates/aether-render/src/runtime/mod.rs`), in the order the render capability received the mail, and that vector is the painter order. A lifecycle stage is broadcast to its subscribers as one burst that the scheduler may spread over several workers, so two actors that draw in the same stage reach the renderer in an order that can differ from frame to frame.

**Input goes to every subscriber.** Both window backends publish through one line, `ctx.fanout(self.subscribers.recipients::<K>(window), event)` (`crates/aether-window/src/runtime/desktop/mod.rs`, `crates/aether-window/src/runtime/synthetic/mod.rs`), and `recipients` is the whole subscriber set for that window. With a camera controller live, typing into a console also moves the camera, and a wheel over a scrolling panel also zooms the scene behind it.

**Text is filed under the wrong actor.** `aether.text` turns a text draw into glyph quads and sends them to the renderer itself (`emit_draw` in `crates/aether-text/src/runtime/layout.rs` calls `ctx.send::<RenderCapability>`), so every glyph batch arrives as mail from `aether.text`.

**The actor tree has no order.** A route record is a name and a lifecycle (`RouteRecord` in `crates/aether-substrate/src/mail/registry/mailbox/route.rs`). The registry keeps nothing that says which of two siblings came first.

The aim that shapes the decision: the number of different ways to solve this problem is kept as small as possible. There is one way to order and one way to place.

Prior decisions this one meets:

- **ADR-0117** made draw order inside one widget root structural and left order between roots unbuilt under "Ordering escape hatch (deferred)". It rejects an absolute key. This ADR builds the order between actors without one.
- **ADR-0164 §4** states the publish rule: a window event is fanned out to `recipients(window, K::ID)`. This ADR changes that rule for pointer, key and text events.
- **ADR-0079 §7** retires an actor's name when it closes; nothing registers it again. Section 6 leans on it.
- **ADR-0099** makes a mailbox id the fold of its path, segment by segment (`lineage_mailbox_id` in `crates/aether-substrate/src/mail/registry/names.rs`). Section 5 leans on it.

Issues #7513 (draw order) and #7514 (input) hold earlier code reading. Where their plans differ from this ADR, this ADR is the decision.

## Terms

- **Sequence.** The order of one parent's children, and of the root actors among themselves.
- **Position.** The opaque, comparable value a reader gets for one actor: where it stands in the whole tree.
- **Layer.** An empty child created only to hold a place in its parent's sequence. Content is created beneath it.
- **Member.** An actor that has stated a pointer region to the window (section 8).
- **Key focus.** The one slot in the window that narrows who hears keys (section 9). It is separate from the sequence. Plain "focus" is not used for it: at this mailbox `aether.window.focus` (`FocusWindow`) and `aether.window.focus_changed` (`WindowFocus`) already name operating-system window focus.

## Decision

### 1. A parent's children have a sequence, and it is creation order

The actor tree is an ordered tree. The children of one parent are in the order they were created, and the root actors are in the order they were created. That is the whole rule.

Nothing else orders actors. There is no reorder verb, no relation one actor states about another, no rank, no author-written number, and no ordering declaration on a type. It works as a list of items in a document does: the order they were written is the order they are visited.

The sequence is a property of the tree. The registry holds it and says nothing about what it means.

### 2. Each reader gives the sequence its meaning

There is one sequence. Readers decide what to do with it.

- **The renderer** paints in sequence: a child over its parent, a later sibling over an earlier one. A subtree moves as one, so everything beneath an actor lies between that actor and its next sibling.
- **The window** reads the same sequence in reverse for pointer input: the frontmost member under the pointer is the one drawn last.
- **Any other reader** may use the sequence or ignore it.

The engine has no z-index. If a reader ever needs an order that differs from the sequence, that exception belongs to that reader and its own state, as a document's paint and tab orders are exceptions to its one document order. None is proposed here.

Inside one actor nothing changes. An actor's own batches keep the order it sent them, so a widget root's composition order (ADR-0117) is still its own.

### 3. Placement is by structure: layers

A parent that needs control over where things stand creates its layers first, in the order it wants, and creates content beneath the right layer whenever the content arrives.

```
app
├── backdrop     layer, created first
├── world        layer
├── hud          layer
└── overlay      layer, created last
    └── console  content, created at any time, always over hud
```

"First", "last" and "between" all become "beneath which layer". The layers are fixed for the life of their parent. The content beneath them is what comes and goes.

**Roots.** Boot order is the root sequence: the chassis composition chain in the order it is written, then the boot list in the order it is written. The chain is claimed in order (`boot_passives`, pass 1, `crates/aether-substrate/src/chassis/builder/boot_passives.rs`) and each boot list entry is awaited before the next (`boot_standard`, `crates/aether-chassis/src/boot.rs`). A root created later is last. An application that needs more than that composes one root whose children are its layers.

What a layer needs from the engine today, read from the code:

- **Content can be created beneath an existing actor by mail.** `aether.component.spawn` carries `parent: Option<ErasedActorPath>` (`Spawn` in `crates/aether-kinds/src/lib.rs`), the MCP `spawn` tool passes it through, and the instance is named `parent/NS:key` (`PreparedLoad::name` in `crates/aether-component/src/component/runtime/load.rs`). The MCP `load_component` tool always spawns at the root (`parent: None` in `crates/aether-mcp/src/tools/components/load.rs`), so content for a layer is published and then spawned.
- **The parent must be `Live`** (`placement_under`, same file).
- **The content type must name the layer's type.** A spawn beneath a parent is refused unless the spawned type declares `child_of(..)` naming the parent's type, and the type must be instanced (`placement_key` and `child_refusal`, `crates/aether-component/src/component/runtime/`). A type may hold several placements, `instanced, root, child_of(A, B)`. So content that is to sit beneath a layer declares that layer's type, and a shared layer type that many content types name is follow-on work this decision creates.
- **A native parent** spawns its layers with `NativeCtx::spawn_child`, which takes only its own actor as the parent and requires `ChildOf` and `Instanced` of the child. A native layer is one native actor with an inbox and no handlers.
- **A guest parent** spawns its layers as inline children. An inline child is an alias route onto its parent's instance (`RouteLifecycle::Alias`), so a guest layer costs one route record and no instance of its own. A layer loaded as its own component costs a whole guest instance.

### 4. Order is applied where results are gathered

Mail sent in an order is not processed in that order, so no rule here sequences deliveries.

- **Drawing.** Every actor draws during the frame in any order. The renderer commits the frame when the engine-only `Frame` mail arrives (`on_frame` → `commit_scene`), and the driver sends `Frame` only after the frame's stages have settled (`run_frame_advance` then `send_render_and_drain` in `crates/aether-chassis-desktop/src/driver/mod.rs`). At that commit the renderer groups the batches by sender (`ctx.sender()`, stamped by the host) and sorts the groups by position.
- **Input.** Each pointer event has one recipient, and key events are routed by the key focus slot, so there is nothing to order.

```rust
// main: one vector, receipt order
state.overlay_frame.push(OverlayBatch::shapes(mail));

// plan: filed under its sender; sorted once, at commit
state.overlay_frame.file(ctx.sender(), OverlayBatch::shapes(mail));
// commit_scene: groups by position, back to front; inside a group, arrival order
```

The sequence does not depend on presence. An actor that submits nothing in a frame is an empty group and nothing shifts. Draws stay immediate mode (ADR-0105): an actor that skips a frame vanishes for that frame. Under `replay_cache_when_idle` the replayed list keeps its groups. A `LifecycleAdvance` that is warn-dropped sends no `Frame`, so no partial set of draws is ever sorted. Retained draws (ADR-0246) are world-space sets ordered by depth and by their program, and this ADR does not touch them.

### 5. Storage: one birth serial on the route record

The registry stamps every route record with a birth serial when the record is first inserted. The serial is not optional, is never written by an actor, and never changes.

```rust
// main: crates/aether-substrate/src/mail/registry/mailbox/route.rs
pub(super) struct RouteRecord {
    pub(super) canonical_name: ErasedActorPath,
    pub(super) lifecycle: RouteLifecycle,
}

// plan
pub(super) struct RouteRecord {
    pub(super) canonical_name: ErasedActorPath,
    pub(super) born: BirthSerial,
    pub(super) lifecycle: RouteLifecycle,
}
```

Four arms of `apply_batch_locked` (`mailbox/apply.rs`) insert a record for the first time, and each draws the next serial: `PreparedSpawn`, `ReserveStarting`, `PublishLive`, and `PublishAlias` when the alias is new. The counter is staged through the batch and committed with it, as `next_activation_token` is, so a refused batch draws nothing.

```rust
// main: PreparedSpawn
let record = RouteRecord { canonical_name: commit.canonical_name, lifecycle: RouteLifecycle::Starting { token } };

// plan
let born = BirthSerial::next(&mut next_birth_serial);
let record = RouteRecord { canonical_name: commit.canonical_name, born, lifecycle: RouteLifecycle::Starting { token } };
```

Three sites build a new record over an existing one from its parts and must carry the serial across: `PromoteStarting` and the same-alias branch of `PublishAlias` in `apply.rs`, and `promote_locked` in `mailbox/birth.rs`. The other writers (`RetireAlias`, `RepublishContract`, `DropMailbox`, `InstallSeize`) clone the record and change its lifecycle, so they carry it with no edit.

```rust
// main: PromoteStarting reads the name from the reserved record
let record = RouteRecord { canonical_name, lifecycle: RouteLifecycle::Live { endpoint: endpoint.clone(), contract } };

// plan: and the serial with it
let record = RouteRecord { canonical_name, born, lifecycle: RouteLifecycle::Live { endpoint: endpoint.clone(), contract } };
```

The stamp is taken at reservation, when the route is `Starting`, and kept when it becomes `Live`. The sequence is therefore the order in which the registry saw the requests, and how long each `init` takes does not change it. The registry owner applies batches on one thread, so that order is total.

Ancestry needs nothing new. A mailbox id is the fold of its path one segment at a time (`lineage_mailbox_id`), so the id of every ancestor falls out of folding the actor's own canonical name, and a path has at most `MAX_SCOPE_PATH_DEPTH = 8` segments.

A reader asks for a position through one new read on `NativeCtx`, beside `actor_path`:

```rust
// main: crates/aether-substrate/src/actor/native/ctx/registry.rs
pub fn actor_path(&self, reference: ErasedActorRef) -> ErasedActorPath

// plan
pub fn position(&self, reference: ErasedActorRef) -> Position
```

A `Position` holds one step per path segment, root first, each step the birth serial of that prefix. Positions compare step by step. A parent's position is a prefix of its child's, so the parent sorts behind the child, and two siblings differ at the step that holds their own serials. The read is at most 8 hash probes in one loaded snapshot of the route view and takes no lock. `Position` is opaque and ordered: it exposes no `MailboxId`, has no constructor outside the registry, and is not a kind field, so it is never mailed.

Mail dispatch does not read the serial. `route_lookup` (`mailbox/resolve.rs`) reads `lifecycle` only. The cost on that path is 8 more bytes per route slot. This protects the renderer and the window, the two readers that would otherwise each keep their own order, and it touches the registry's birth arms, which run once per actor.

Whether every prefix of a live actor's path holds a route record:

- A native child is spawned by its own parent's handler (`spawn_child`), so the parent's record exists.
- A component spawned beneath a parent requires the parent to resolve `Live`.
- An inline alias requires its target parent to be `Starting` or `Live` (`PublishAlias`).
- A closed ancestor keeps its record as `Dropped`, serial included.

So the three creation paths each build on an existing record. The registry itself does not check it: `ReserveStarting` and `PublishLive` accept any name that passes the path grammar. The plan adds that check to the stamping arms (one probe of the parent prefix at birth), so `position` has no failure to report. This is open question 3.

### 6. A position is for life

Every actor has a position from the moment its route is reserved. No actor is unplaced and no draw is refused for want of one.

A position stands across a republish. The mailbox and its record stay (`RepublishContract` clones the record), so a republished component keeps its place.

A closed actor never returns to its position, because it never returns at all. `DropMailbox` leaves the record as `Dropped`, and `PreparedSpawn` and `ReserveStarting` both refuse a birth whose id already holds any record, `Dropped` included (`route_conflict_failure` answers `SubnameRetired`; ADR-0079 §7). A component that is dropped and brought back is spawned under a new key, which is a new actor with a new serial, so it is last among its siblings. The code leaves no choice between keeping and renewing a position here.

One path does reuse a name: a `Starting` reservation that is cancelled removes its record (`CancelStarting`), and a retry under the same name is stamped again, later. That actor never lived, so the plan lets it take the later serial. This is open question 4.

### 7. Senders that are not ordinary actors

The sort has no special case. A batch sorts where its sender's position puts it, and a batch with no sender has the empty position, which is a prefix of every other and so sorts behind everything. Read from the code, and proposed:

| Draws from | Sender the renderer sees | Where it sorts (proposed) |
|---|---|---|
| MCP `send_mail` | `aether.rpc.server`, which delivers each call with `deliver_detached` (`crates/aether-rpc/src/server/runtime.rs`) | At that root's boot position: behind every component in the boot list and every later load |
| MCP `send_mail_traced` | `aether.rpc.server` too: `aether.trace` delivers with `deliver_forwarded`, which pins the reply target to the inbound one (`crates/aether-trace/src/runtime.rs`) | The same |
| `capture_frame` pre-mails and after-mails | `aether.render` itself, which delivers them with `deliver_detached` (`crates/aether-render/src/runtime/mod.rs`) | At the render capability's boot position |
| A harness or driver root push | None: `push_root` stamps `Source::NONE` or a settling inbox (`crates/aether-substrate/src/chassis/builder/root_pusher.rs`), and `NativeCtx::sender` answers `None` for both | Behind everything |

`ctx.sender()` reads the mail's reply target (`crates/aether-substrate/src/actor/native/ctx/inbound.rs`), which is why a forwarded mail keeps its original sender.

The effect is that a draw mailed straight to the renderer from a session, a capture or a test lands behind every component. A session that wants a draw at a chosen place mails an actor beneath the right layer and that actor draws. Whether that is acceptable for sessions is open question 2.

### 8. Pointer input

A member states the region in which it takes the pointer: a rectangle in window pixels, or the whole window. The statement is mail to `aether.window`. A member that draws nothing can still take the pointer: the camera controller states the whole window.

A pointer event goes to the frontmost member whose region holds the point, and to nobody else. Frontmost is by position, read through the same `NativeCtx::position`. Subscriptions still decide which kinds an actor is sent.

**A press owns its release.** The window remembers which member was sent each button press. The drag's moves, wheel and release go to that same member wherever the pointer is by then. A window that loses operating-system focus forgets its owners, because those releases never arrive. An owner that closes takes its entries with it.

The window never gives key focus on a press. The member that receives the press decides whether to take it.

Events that are neither pointer nor key events keep the ADR-0164 §4 fan-out: `Modifiers`, `WindowSize`, `WindowFocus`, `WindowOpened`, `WindowClosed`, `WindowMenuActivated`.

### 9. Key focus

Key focus is separate from the sequence. It is state the window holds: one slot, which is empty or holds one actor and a scope. There is no stack and no history.

- **An actor takes key focus for itself only**, never for another actor, with one of two scopes: itself alone, or itself and everything beneath it.
- **The latest take wins.** A descendant may take key focus while an ancestor holds it for the subtree; the slot then holds the descendant.
- **Keys go to every key subscriber inside the holder's scope.** When the slot is empty, every key subscriber hears keys, which is today's behaviour. Key events are `Key`, `KeyRelease`, `TextInput` and `ImePreedit`.
- **A release, or the holder's close, empties the slot.** Key focus is never handed back. A parent whose child gives it up takes it again itself if it needs it, and the child tells its parent by ordinary mail.
- **Nothing takes key focus merely to receive keys.** Taking it means everyone outside the scope stops hearing them.
- **The window tells an actor when it gains key focus and when it loses it.**
- **The pointer is unaffected.** A press goes to the frontmost member under it by position, whoever holds key focus.

| Moment | Slot | Hears keys |
|---|---|---|
| Playing | empty | every key subscriber: the camera controller |
| The console opens and takes key focus, itself alone | console | the console |
| The console closes or releases | empty | every key subscriber |

The kinds, proposed. They follow the mailbox's existing shape (a sender-acting command, as `aether.window.subscribe_self` is, and published notices, as `aether.window.focus_changed` is) and carry "key" in the name to stay clear of `aether.window.focus`:

```rust
// plan (proposed)
#[aether_data::kind(name = "aether.window.take_key_focus", copy, eq)]
pub struct TakeKeyFocus {
    pub scope: KeyFocusScope,
}

pub enum KeyFocusScope {
    Actor,
    Subtree,
}

#[aether_data::kind(name = "aether.window.release_key_focus", copy, eq)]
pub struct ReleaseKeyFocus;

#[aether_data::kind(name = "aether.window.key_focus_gained", copy, eq)]
pub struct KeyFocusGained;

#[aether_data::kind(name = "aether.window.key_focus_lost", copy, eq)]
pub struct KeyFocusLost;
```

The taker is the mail's sender, so a take with no local sender is refused, as `subscribe_self` refuses one. A release from an actor that is not the holder changes nothing.

How the window routes keys today, and what the slot needs, read from `crates/aether-window/src/runtime/subscribers.rs`:

- Every published kind has its own `KindSubscribers<K>`: one map of subscribers to all windows and one per window, both keyed by the subscriber's `ErasedActorRef`. `recipients(window)` chains the two, and `publish` hands the whole iterator to `ctx.fanout`. There is no filter between the table and the send.
- The table holds references, with no path beside them. The window can read a subscriber's canonical path with `NativeCtx::actor_path`, one probe of the route view.
- A subscriber is inside a subtree scope when the holder's canonical path is a prefix of the subscriber's ending on a segment boundary. The same test on `Position` values is a prefix test.
- The window already monitors every subscriber (`Holder` and its `MonitorHandle`) and drops its rows on the notice. The slot's holder is watched the same way, which is how the holder's close empties the slot.

```rust
// main: crates/aether-window/src/runtime/desktop/mod.rs
ctx.fanout(self.subscribers.recipients::<K>(window), event);

// plan: for the four key kinds only
ctx.fanout(self.subscribers.recipients::<K>(window).filter(|subscriber| self.key_focus.admits(subscriber)), event);
```

Whether `admits` reads paths on every key event or keeps the admitted set, rebuilt on a take, a release and a subscribe, is an implementation choice; the set is the cheaper read.

### 10. Text is filed with its owner's shapes

`aether.text` sends its glyph quads with `NativeCtx::forward_to` (`crates/aether-substrate/src/actor/native/ctx/send.rs`), which keeps the inbound mail's sender as the forwarded mail's sender. The renderer then files each glyph batch under the actor that asked for the text. Atlas `CreateTexture` and `UpdateTexture` sends stay the text capability's own.

```rust
// main
ctx.send::<RenderCapability>(&draw);

// plan
ctx.forward_to(render, &draw);
```

## Consequences

- **A cross-actor overlay can be drawn.** It is created beneath a layer that was created after the scene's layer, and it is over the scene on every frame.
- **Nobody declares an order.** Existing drawers (`aether-widget`'s `emit_layer`, the kit bundle tile, the fixtures) change nothing to be ordered. They change only if they must stand somewhere other than where creation put them, and then they move beneath a layer.
- **ADR-0117 is completed in part.** Order between roots, and between an actor and a child that draws for itself, is built, with no key and no edges. A widget subtree still reaches the renderer through one sender.
- **Keys can be kept from the scene.** The console takes key focus while it is open and the camera controller stops hearing keys, with no change to the camera controller.
- **ADR-0164 §4 changes for pointer, key and text events**, and **ADR-0105** for the sender of glyph quads. The lines are written on those ADRs when this is implemented.
- **Guides.** `rendering.md`, `window.md`, `input.md`, `text.md`, `widgets.md` and `foundations/actor-model.md` change with the implementation.

### What is given up

- **An existing child cannot be moved among its siblings.** Order that changes while running lives inside one actor's own draws, which that actor already controls.
- **A root that is dropped and brought back is last.** Its old name is retired and the new instance is a new actor (section 6). Content that must keep its place across being closed and created again sits beneath a layer.
- **`depends(R)` forces `R` to be created first.** Where that conflicts with the wanted order, the two actors sit beneath layers that carry the order.
- **Two roots loaded by mail at the same time land in the order the registry saw them.** That order can differ between runs.
- **Lineage now does two jobs.** It already decides lifetime, and now it decides order, so actors grouped for order are grouped for lifetime.
- **Key focus is not handed back.** A parent that forgets to take it again after its child releases leaves the slot empty, and keys then reach everything behind it. A stack of holders would not have that failure, and is the adjustment to make later if this one hurts.
- **There is no explicit ordering.** It is left out on purpose. It could be added later without breaking anything built on creation order.

### Limits

- **One sequence per application.** The render scene is application-scoped: `on_frame` commits one scene and records it for every dirty window, and no draw kind names a window. A member's region is read in each window's pixels.
- **No bubbling.** A parent cannot decide per event after its child looked.
- **Hover.** A member that highlights under the cursor keeps its highlight when the cursor enters a region in front of it, because moves stop arriving and nothing tells it.
- **Text and shapes inside one actor.** A plate that must cover text the same actor sent earlier is an order between two batches of one group that reach the renderer through two recipients. Position does not solve it. Until text has a shape that does, the plate is a child, which is over its parent's whole group.

## Open questions

**1. A key held, or a composition in progress, when key focus changes.** A subscriber that heard a key go down and then falls outside the scope never hears it come up. Options: the window remembers who was sent each key-down and sends them the release, as it does for a button press; or a subscriber treats `KeyFocusLost`, and a take by another actor, as the release of everything it holds. The second does not reach a subscriber that never held key focus, such as the camera controller. Lean: the window remembers, so the rule for keys is the rule for buttons. What happens to an `ImePreedit` composition when the slot changes is not decided.

**2. Session draws.** Section 7 puts MCP, capture and harness draws behind every component with no special case. The alternative is one stated place for them in front of everything, which is a special case in the renderer's sort. Lean: no special case.

**3. A prefix with no record.** The registry accepts a multi-segment name whose parent prefix holds no record. Options: the stamping arms refuse it (one more probe per birth), or `position` treats a missing prefix as a step that sorts first. Lean: refuse at birth, so the read is total.

**4. A cancelled reservation retried under the same name.** It is stamped again and takes the later serial. The alternative is to remember the first serial across the cancel, which needs state the registry does not keep today. Lean: the later serial.

**5. The pointer region and its kind.** A rectangle or the whole window; a drawn shape is not a rectangle. Lean: those two only, and a list of rectangles when a case needs it. The kind name is not settled.

**6. A shared layer type.** Content must declare `child_of(..)` naming its layer's type (section 3). Whether the engine ships one layer type for content to name, and in which crate, is not decided.

**7. One slot for the application or one per window.** Keys arrive for one window, and subscriptions are per window or for all. The decision says one slot. Lean: one for the application, held by the manager `aether.window`, matching the one sequence per application under Limits.

## Not verified

- Whether a component can be spawned by mail beneath an inline alias. `placement_under` needs the parent to resolve `Live`; how `resolve_live` answers for an alias was not read to the end.
- Whether a guest type's `child_of(..)` may name a native type, so that a guest can be spawned beneath a native layer. The host compares namespaces from the module's lineage section; the derive side was not read.
- Where in a component load the `PreparedSpawn` effect is submitted relative to module compilation. If it follows compilation, two loads sent together are ordered by how long each compiles.
- That every caller of `PreparedRoute::named` passes a one-segment name. That constructor hashes the whole name where a multi-segment path must be folded.
- That `spawn_substrate`'s `components` list reaches the same awaited loader as the boot list.
- That a harness reply inbox's source never reads as an actor sender. `NativeCtx::sender` answers an actor only for `SourceAddr::Component`, or for a reply; the inbox's variant was not read.
- What republishing the route view costs per birth with the added field. The record grows by 8 bytes and is cloned where it is today.
- How actors from another journal are born.

## Alternatives considered

- **Key focus derived from the sequence** (the frontmost member that takes keys has them). It needs no state, and it fails for two text fields side by side beneath one parent: one is always in front and nothing can be moved.
- **A stack of key focus holders.** It hands focus back when a holder leaves, and it is more state than one slot. Left as the later adjustment.
- **The window gives key focus to whatever is pressed.** The window would decide what a press means for an actor that may not want keys at all.
- **An explicit relation between actors, stated by mail or on the type.** A second way to order beside the tree, with cycles and unordered pairs to refuse. Layers say the same thing with structure that already exists.
- **A reorder verb for siblings.** It makes the sequence state that any actor can change at any time, and every reader must then handle a change mid-frame. Changing order stays inside one actor.
- **A number, band or enum on the draw kinds.** ADR-0117 already rejects an absolute key: every author must agree on one scale.
- **A sequence per purpose (one for drawing, one for input).** A click could land on something drawn underneath what the user sees.
- **The order kept by the window or by the renderer.** Each would learn of every birth and close to keep a list the registry can stamp for 8 bytes, and the other reader would depend on it.
- **Stamping at `Live`.** Order would follow how long each `init` takes.
- **Order from `#[actor(depends(..))]`.** An edge that means "must be live first" would also mean "draws under", and most such edges are not paint relations.
- **Draw on `Present`, or a new lifecycle stage for overlays.** Stage order used as draw order; it stops working when a second actor does the same.
- **Routing every draw through an owner actor.** Puts every draw one hop behind, which is the text problem for everything.
- **Decentralized input routing.** Each actor passes events to its children. A hop per level, and a parent that traps silences its subtree with no message.
