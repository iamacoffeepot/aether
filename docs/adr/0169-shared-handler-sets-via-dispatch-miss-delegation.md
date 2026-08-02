# ADR-0169: Shared Handler Sets via Dispatch-Miss Delegation

- **Status:** Accepted
- **Date:** 2026-08-01

Adds a composition axis to the `#[actor]` / `#[runtime]` authoring surface of **ADR-0033** (handler-driven inputs manifest) and **ADR-0074** (unified actor model), leaving the reply classes of **ADR-0112** / **ADR-0134** and the addressing of **ADR-0119** / **ADR-0166** untouched.

## Context

One `#[actor] impl WasmActor for X` block is an actor's entire receive surface. There is no way to say "and also the handlers every sibling of this kind carries," so a family of near-identical actors re-declares its shared block per member. Three sites in the tree pay for that absence today.

**Window instance runtimes.** `crates/aether-window/src/runtime/desktop/instance.rs` and `crates/aether-window/src/runtime/synthetic/instance.rs` are 62 lines each and differ in three: the doc comment, the import order, and the type name. Both declare the same seven handlers, and every body is a one-line delegation to a shared free function already factored into `runtime/instance.rs`. What is duplicated is the handler declarations themselves, not the logic behind them.

**Window manager subscription handlers.** `runtime/desktop/mod.rs` and `runtime/synthetic/mod.rs` carry a verbatim 55-line run of six handlers — `on_subscribe`, `on_subscribe_self`, `on_unsubscribe`, `on_unsubscribe_self`, `on_unsubscribe_all`, `on_inject` — in the split authoring shape (`state: &mut Self::State` rather than a `self` receiver). Same family, same absence, one level up.

**Widget handler blocks.** Eleven widgets under `crates/aether-kit-widget/src/set/` each re-declare the handlers that absorb ambient state pushed down by the parent — geometry, theme, focus, hover, and enabled/read-only state. The bodies are byte-identical across most of the family:

| handler | identical across | body |
| --- | --- | --- |
| `on_frame` | 11 of 11 | `self.frame = frame` |
| `on_set_widget_state` | 9 of 11 | `self.apply_control_state(ctx, set.state)` |
| `on_hover_gained` | 9 of 11 | `self.state.set_hovered(true)` |
| `on_hover_lost` | 8 of 11 | `self.state.set_hovered(false)` |
| `on_set_theme` | 8 of 11 | `self.theme = set.theme` |
| `on_focus_gained` | 8 of 11 | `self.state.gain_focus()` |
| `on_focus_lost` | **0 of 11** | each clears its own arm / drag / composition state |

The split is the load-bearing fact. Most of the block is shared, and one member of it is shared by nobody: every widget's `on_focus_lost` calls `self.state.lose_focus()` and then cancels whatever activation state that particular widget tracks. An all-or-nothing shared block covers the window runtimes and strands most of the widget family, so **per-member override is a requirement, not a refinement**.

This is a missing primitive rather than sloppiness: it reproduces every time a widget or a window runtime is added, and the duplicate-code gate cannot act on it. jscpd does *see* it — of the 29 clones it currently reports across `crates/`, five are exactly this class: both window-runtime pairs, `button.rs` against `radio.rs`, and `segmented.rs` against each of `toggle.rs` and `virtual_list.rs`. What the gate cannot do is fail on them, because `.jscpd.json` sets a tree-wide duplicated-line percentage (0.5%) and the workspace sits at 0.26%. A real but small clone class never moves that ratio, and lowering the threshold far enough to catch a 28-line block would flag inherent sibling similarity across the whole tree. The gate measures aggregate ratio; this is a structural absence, so the gate is not the lever.

### What a shared block has to plug into

`#[actor]` emits three things per handler kind, and a shared set must contribute to each:

1. **The dispatch table** — `build_dispatch_body` emits an if-chain over `<K as Kind>::ID` inside the inherent `__aether_dispatch`, falling through to the `#[fallback]` or `DISPATCH_UNKNOWN_KIND`.
2. **A marker impl** — one `impl HandlesKind<K> for X {}` per kind (ADR-0075), which gates the typed-resolver send path `ctx.actor::<R>().send(&k)` at the call site.
3. **The inputs manifest** — a record per handler in the `aether.kinds.inputs` custom section, carried as the const pair `__AETHER_INPUTS_MANIFEST_LEN: usize` / `__AETHER_INPUTS_MANIFEST: [u8; LEN]` that `export!` pins (ADR-0033).

Input-stream subscription needs no separate plumbing: since issue #403 the substrate derives subscriptions from the inputs manifest after the mailbox registers, so a kind that reaches the manifest is subscribed by that fact alone.

Two constraints shape the mechanism. A proc macro cannot read a declaration it is not applied to, so `#[actor]` cannot learn a shared set's kinds by inspection. And the marker impls cannot be blanket-implemented from the set's own definition: `impl<T: WidgetDefaults> HandlesKind<WidgetFrame> for T {}` places the type parameter `Self` ahead of the first local type in the trait reference, which the orphan rule rejects.

The adopters differ in whether they need marker impls at all. Widget parents address children by name through `ctx.child(name)`, whose `RelativeMailbox::send<K: Kind>` carries no `HandlesKind` bound, so the widget kinds are never gated. The window instances are reached through the typed facade in `aether-window/src/lib.rs`, which is bounded `WindowInstance: HandlesKind<K>` throughout. The mechanism therefore has to emit them, and cannot make emitting them the adopter's problem.

## Decision

### 1. A handler set is a trait carrying `#[handler]` methods with default bodies

`#[handler_set]` on a trait declares a reusable block of handlers. The trait's required methods are the accessors the shared bodies need; its `#[handler::<class>]` methods carry the shared behavior as default bodies.

```rust
#[handler_set]
pub trait WidgetDefaults {
    fn widget_frame(&mut self) -> &mut WidgetFrame;
    fn widget_state(&mut self) -> &mut InteractionState;
    fn widget_theme(&mut self) -> &mut Theme;

    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        *self.widget_frame() = frame;
    }

    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.widget_state().set_hovered(true);
    }
}
```

Handler methods are parsed by the same `handler_parse` code path `#[actor]` uses, so reply classes, `Multi<K>` markers, and the ADR-0134 class rules apply unchanged inside a set. The expansion emits the trait with the `#[handler::*]` attributes stripped, plus three hidden items:

- a provided method `__aether_handler_set_dispatch(&mut self, ctx, mail) -> u32` whose body is the set's own if-chain, returning `DISPATCH_HANDLED` or `DISPATCH_UNKNOWN_KIND`;
- the set's receive surface in the form its transport reads — for a wasm set the associated const `__AETHER_HANDLER_SET_MANIFEST`, its inputs-manifest bytes in the same record encoding; for a native set the provided method `__aether_handler_set_capabilities`, the `HandlerCapability` rows an adopter splices into its own `capabilities` / `measured_kinds`;
- for a native set, an exported `macro_rules!` bridge emitting the set's `impl HandlesKind<K> for $ty {}` markers and matching `HandlerEntry` inventory records, which exists solely because the orphan rule forecloses the blanket impl. A wasm set emits none: nothing on that transport reads the marker.

### 2. Adoption is `#[actor(handler_set(T))]`, and the local block is matched first

An adopter names the set in its actor attribute and implements the trait's accessors.

```rust
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for ToggleWidget { /* only toggle-specific handlers */ }
```

`#[actor]` emits its existing output plus: the local dispatch chain, then a delegation to `<Self as T>::__aether_handler_set_dispatch`, then the existing tail; the set's manifest const added to the length sum and copied into the manifest array; and one invocation of the bridge macro.

```
match local arms         -> DISPATCH_HANDLED
else set dispatch        -> DISPATCH_HANDLED
else #[fallback] / DISPATCH_UNKNOWN_KIND
```

Local-first is the ordering of record. It costs nothing (the local chain is emitted anyway), it keeps the adopter's own declarations authoritative over anything inherited, and it makes a set a strictly additive tail on the existing table rather than a restructuring of it.

### 3. Override replaces the trait's default body

A member that differs is overridden the ordinary Rust way, in the adopter's `impl T for X` block:

```rust
impl WidgetDefaults for ToggleWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame { &mut self.frame }
    fn widget_state(&mut self) -> &mut InteractionState { &mut self.state }
    fn widget_theme(&mut self) -> &mut Theme { &mut self.theme }

    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.clear_arms();          // toggle-specific
    }
}
```

The kind stays owned by the set — one dispatch arm, one marker impl, one manifest record — and only the body changes. Overriding by re-declaring the handler in the `#[actor]` block instead is **not** the override path: that emits a second `impl HandlesKind<K> for X {}` alongside the bridge's, which is a coherence error (E0119). Local and set kinds are disjoint by construction, and the compiler enforces it.

### 4. Sets do not nest, and an actor adopts at most one

`#[handler_set]` rejects a trait that itself names a set. One `handler_set(T)` per `#[actor]`. Both restrictions keep the dispatch order a two-step statement a reader can hold in their head; a family that outgrows one set is evidence for a second set, not for a chain of them. Neither is a wire or ABI commitment, so both can be relaxed later without a break.

### 5. Sets support both authoring shapes

A set's handlers are written in whichever shape its adopters use: the `&mut self` receiver of a plain actor, or the split `state: &mut Self::State` first parameter of a `type State = …` actor (the shape the window managers use). `#[handler_set]` reuses `extract_native_actor_handler_kind` / `rewrite_self_state_first_param`, so the split shape needs no new rule — a set is split or not, and an adopter of the other shape is a compile error at the delegation call.

### 6. Adopters

`WidgetDefaults` in `aether-kit-widget` carries the shared handlers of the table above, with `on_focus_lost` declared in the set (its `self.state.lose_focus()` half is universal) and overridden by every widget. The window instance runtimes adopt a native set carrying all seven handlers, reducing each of the two files to its type name, doc comment, and `#[actor(handler_set(..))]` line. The window managers adopt a second, split-shape set carrying the six subscription handlers.

## Consequences

### Positive

- **Adding a sibling stops meaning copying a handler block.** The next widget declares its behavior and its draw; the next window runtime declares its type. That is the whole point of the issue this ADR answers.
- **Override is visible and typed.** An overridden member is a trait method with a fixed signature, so a mismatched reply class or ctx marker is a compile error at the override rather than a silently divergent second arm.
- **The manifest stays true by construction.** A set's handlers reach `aether.kinds.inputs` through the same record encoding as local ones, so `describe_component` reports an adopter's full receive surface and input subscription keeps working with no additional plumbing.
- **Five of the workspace's 29 reported clones go away at the source**, rather than by moving a threshold that would then flag legitimate sibling similarity. The gate is left measuring what it is good at — aggregate ratio — instead of being tuned to chase a structural absence.

### Negative / limits

- **A new authoring concept.** An actor's receive surface can now live in two places, and a reader who sees only the `#[actor]` block no longer sees every handler. The `handler_set(..)` term in the attribute is the signpost, and `describe_component` still reports the merged surface.
- **Re-declaring a set kind locally fails as E0119 rather than as a pointed macro diagnostic.** `#[actor]` cannot read the set's kinds, so it cannot say "`FocusLost` is owned by `WidgetDefaults` — override it there." The conflicting-implementations error does name the trait and the kind, so the failure is real and locatable, but it is not the message the macro would write.
- **A `macro_rules!` bridge in the expansion.** It exists only to route around the orphan rule for the marker impls. It is generated, hidden, and invoked by generated code, but it is a second expansion mechanism in a crate that otherwise emits plain items. Two consequences ride with it. The invocation must be unqualified — a macro-expanded `#[macro_export]` macro named as `crate::name!` from inside its own crate trips `macro_expanded_macro_exports_accessed_by_absolute_paths` (rust-lang issue 52234), which is the case every in-tree adopter is in; the unqualified form resolves through the crate-root macro prelude and is order-independent, so an adopter declared above the set's own `mod` line still sees it. And a `macro_rules!` pastes paths at the use site, so a native set's kind types need spellings that resolve from every adopter.
- **Accessor boilerplate replaces handler boilerplate for the widget family.** A widget trades roughly six handler declarations for three accessor methods. The window family pays nothing, since its shared bodies already delegate to free functions.
- **Non-generic adopters only, initially.** A wasm adopter's manifest length is const arithmetic over `<Self as T>::__AETHER_HANDLER_SET_MANIFEST.len()`; a generic adopter is out of scope here. Every actor in the tree today is a concrete type.

### Neutral / forward

- **No wire, ABI, or custom-section change.** Set records use the existing `aether.kinds.inputs` encoding, so no section version bump and nothing to migrate on the host side.
- **Native and wasm both gain it.** `#[runtime]` reaches the same expansion path as `#[actor]`, so the window instances adopt the same primitive the widgets do.
- **Adoption is opt-in per actor.** No existing `#[actor]` block changes meaning; a family adopts when someone factors a set for it.
- **Handler-set-aware diagnostics are the natural follow-on** if the E0119 path proves confusing in practice — most directly by having the bridge macro accept the adopter's local kind list and emit a `const _: () = panic!(..)` naming the collision.

## Alternatives considered

- **Splice the shared handlers in with a `macro_rules!` wrapper around the impl block, with an `except(..)` opt-out list per adopter.** Needs no derive-crate change, but states each override twice — once in `except(..)`, once as the handler — and the two drift silently: a kind listed but never overridden vanishes from the actor with no compile error, warn-dropping at runtime.
- **The same splice with no override support at all.** Fully fixes the window runtimes, which are byte-identical. For widgets the shared set collapses to whatever all eleven share, which is `on_frame` alone, leaving the larger half of the motivating duplication in place.
- **A blanket `impl<T: WidgetDefaults> HandlesKind<K> for T {}` from the set's definition.** Rejected by the orphan rule — the `Self` type parameter precedes the first local type in the trait reference. This is what forces the bridge macro.
- **`HandlesKind<K>` supertrait bounds on the set instead of generated markers.** Compiles, but the bounds are unsatisfiable without impls somebody has to write, which pushes the boilerplate back onto every adopter in a less obvious form.
- **A generic base type (`Widget<B>`) holding the shared state and handlers.** Reaches for inheritance the actor model does not have: it fixes the state layout for every member, and a widget's own handlers would have to reach through the base to touch fields it owns.
- **Lower the jscpd threshold and let the gate force the factoring.** The detector already reports these clones; what is missing is a reason to fail, and the only lever is a tree-wide percentage that would have to drop below 0.26% — low enough to fire on inherent sibling similarity everywhere else. A gate that trips on unavoidable resemblance trains people to tune it off.
- **Leave it and copy the block.** The status quo reproduces the cost on every new widget and window runtime, which is precisely the "missing primitive, not sloppiness" reading the issue starts from.
