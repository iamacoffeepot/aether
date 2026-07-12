# ADR-0147: Module Boot Actor and Default Export Slot

- **Status:** Proposed
- **Date:** 2026-07-11

## Context

A wasm module exports one or more actor types (ADR-0096). A `load_component` instantiates exactly one of them: the type named by the export selector, or — if the module opted in with `export!(entry = A, …)` — the entry type when no selector is given (ADR-0138). Nothing else in the module runs.

That contract has no seam for work a module's actors *all* depend on.

The reference kit is where this bites. `aether.kit.console` embeds a monospace face (`console/mod.rs:38-39` — an `include_bytes!` of the TTF beside a separate `const` naming it, a pair free to drift) and registers it with the text capability through `aether.text.load_font_bytes` at wire time. It works, and it is the only actor in the workspace that has a font it can rely on. The widget set has none: `Theme::DEFAULT.font_id` is `0` (`widget/theme.rs:147`), a placeholder into a live id space, and `WidgetPanel` only loads a font at all when one is configured — `if !self.config.font_path.is_empty()` (`widget/panel.rs:912`), through the filesystem-backed `aether.text.load_font`. An unconfigured panel therefore draws its labels with whatever font id `0` happens to be.

The obvious repair — a bootstrapping actor that registers the baseline and sits in the module's `entry` slot — does not hold, because `entry` fires only on a *bare* load. `load_component aether_kit@aether.kit.camera` names a selector, so the entry type never instantiates and the baseline never registers. The slot answers "which actor did you mean?", not "what must always be true?".

Two properties of the runtime shape everything downstream:

- **`init` is synchronous; mail is not.** An actor's `init` runs to completion before any reply can arrive. Anything an actor learns by mail is therefore necessarily absent at construction, and the actor must hold it as an `Option` behind a guard for the rest of its life.
- **Loads are independent.** One module's load has no ordering relationship to another's. A gate on a module's own prologue cannot make module A's fonts exist before module B's widgets ask for them, because B is not waiting on B.

Capabilities that mint session-scoped ids — `aether.text` for fonts, `aether.render` for textures — hand the id back to the single sender that asked. There is no way for a second actor to name that resource. This is the mechanism behind "the console has a font and the widgets do not," and it recurs for every baked asset class the kit grows.

Finally, the module's payload is already paid. A kit cdylib carries its baked bytes whether you instantiate the console, the camera, or nothing at all — one module, one wasm. What is missing is not the bytes but the guarantee that something registered them.

## Decision

### 1. `export!` gains a `boot` slot

```rust
aether_actor::export!(
    boot = boot::Boot,
    default = console::ConsoleOverlay,
    camera::CameraComponent,
    …
);
```

The `boot` type instantiates on **every** load of the module, whatever export selector the caller named. It is not a default and cannot be selected; it is unconditional.

Its cardinality is **one instance per `(engine, module content hash)`**, not one per load. The content-addressed component store (ADR-0116) already supplies the key. The instance is refcounted against the actors loaded from that module and torn down with the last of them. Five selector loads of the kit produce five actors and one `Boot`.

### 2. `entry` is renamed `default`

Semantics are unchanged from ADR-0138: an opt-in designation of what a bare load with no selector instantiates, absent which a bare load of a multi-actor module is an error naming the exports. Only the word changes.

`entry` meant "where the module starts." Once a prologue exists and runs first, that is no longer true of the slot, and `default` names what it actually is — the answer to an omitted selector. ADR-0138's own prose already reads "default entry" throughout.

### 3. The boot slot, module, and type all carry the name

`boot = boot::Boot` — the slot, the module, and the type. The repetition is deliberate. A reader who encounters any one of the three knows the other two without looking, and the cost of the redundancy is three characters against a lookup.

### 4. A boot actor spawns nothing

Instantiating siblings is the `default` slot's job. `boot` runs on every load, so a `boot` that spawned the console would raise a console overlay when the caller asked for the camera. `boot` registers what the module's actors depend on and goes quiet. Nothing holds a reference to it afterwards.

The batteries-included path is preserved by the two slots together: a bare `load_component aether_kit` runs `Boot` (fonts) and instantiates `ConsoleOverlay` (`default`), which is what it did before, now for a stated reason rather than a positional accident.

### 5. Baseline resources are named at the capability that mints them

A capability holding a session-scoped id space gains a name table, a bind kind, and a bound event. For `aether.text`:

- `aether.text.bind_font { name, font_id }` — bind a name to a registered font. Rebinding is legal and is how a game overrides the baseline with its own face.
- `FontRef` grows a `Named(String)` variant beside the existing `Id(u32)` and `Path { namespace, path }`. `DrawText.font_id: u32` widens to `DrawText.font: FontRef`, and `FontMetricsRequest` takes the same `FontRef`.
- `aether.text.font_bound { name, font_id }` — broadcast on every bind, subscribable from an actor's `wire` hook.
- The baseline names are shared consts on the capability — `FONT_UI`, `FONT_MONO` — imported by binder and consumer alike.

An actor draws by naming what it wants. The id never travels, so no actor holds one and no actor waits for one to arrive.

### 6. Readiness is an event, not a load gate

The cap resolves a `Named` reference at use time. An unbound name drops the draw and warns once. Nothing needs to be ordered for drawing to be correct.

Work that needs the font to *exist* — a `FontMetricsRequest` whose result feeds layout, which is what `widget/text_edit.rs` and `widget/set/numeric.rs` already reconcile — subscribes to `font_bound` in `wire` and runs on the event. This is the engine's existing publish/subscribe idiom (ADR-0021/0068), the same mechanism those actors already use for `Tick` and `Key`.

Because a bind is a fact rather than a load, it orders across modules for free. A subscriber is told whether the bind happened before it loaded, after it loaded, or twice.

### 7. The kit's baseline

`aether-kit` bakes two faces behind a `BakedFont { name, bytes, license }` whose name, path, and license text all derive from one literal, and `Boot` registers both and binds them:

- `FONT_UI` — a proportional face. What the widget set draws with; `Theme::DEFAULT.font` becomes `FontRef::Named(FONT_UI)` and the `font_id: 0` placeholder is deleted.
- `FONT_MONO` — the console's monospace face, moved off `ConsoleOverlay`.

`WidgetPanel`'s configured-font override is unchanged in shape: it still loads by path and stamps the theme, now with `FontRef::Id`.

### 8. A baked asset carries its license in the module

Every baked asset contributes its license text to an `aether.licenses` **wasm custom section**, emitted by `export!` alongside the sections it already writes (`aether.kinds`, `aether.namespace`, `aether.kinds.inputs`).

A license file in the source tree — `crates/aether-kit/assets/fonts/SourceCodePro-LICENSE.md` today — discharges the obligation for *source* distribution and nothing else. Components travel as bare bytes: `upload_component` hashes the raw wasm into the content-addressed store (ADR-0116) and the hub hands those bytes to a substrate that never sees the repository. Attribution has to be inside the artifact or it does not arrive.

A custom section rather than a Rust `const` because a `const` nothing references is dead-code-eliminated, while a section is guaranteed to survive, travels with the bytes wherever they are copied, and is readable by a section dump without instantiating the module — the same property the host already relies on to read a component's kind vocabulary before loading it.

The section records attribution. It does not by itself satisfy every license: OFL and Apache-2.0 impose different obligations (Apache-2.0 additionally propagates a `NOTICE`), so adopting a face means reading its terms, not assuming the mechanism covers them.

This supersedes the entry-type clause of ADR-0096 §3 and amends ADR-0138 (`entry` → `default`, which ADR-0138 introduced).

## Consequences

### Positive

- **A module can state what must be true before any of its actors run.** Every actor gets its module's baseline regardless of which selector loaded it, which is the property `entry` could never provide.
- **Cross-module ordering dissolves.** Nobody waits on a load. A bind is broadcast, and a late subscriber is told.
- **No ids travel.** An actor names the resource it wants and the owning capability resolves it, so the `Option`-and-guard tax that any mail-delivered id would impose on every consumer never appears.
- **`default` names what it is.** The slot stops claiming to be an entry point once something else runs first.
- **A latent bug dies.** `Theme::DEFAULT.font_id = 0` is a placeholder in a real id space; `FontRef::Named` makes "unset" representable and removes the sentinel.
- **The kit becomes self-contained.** A bare load and a selector load both yield working text, with no configured font path and no `aether.fs` staging.
- **The named-resource shape generalizes.** `aether.render.bind_texture` / `TextureRef::Named` is the same decision for the first baked image, and the boot slot is where it gets registered.
- **Attribution survives the artifact boundary.** A component uploaded to the content-addressed store carries its baked assets' licenses in the module, so a substrate that never saw the repository still holds the attribution — and the same section serves the first baked image without a second mechanism.

### Negative

- **The module contract grows a third slot.** The macro, the wasm custom sections, the host load path, and `aether-mcp`'s resolution each gain a branch — the same three layers ADR-0138 enumerates, touched again.
- **Per-module-singleton boot is a new host lifetime.** Refcounting a boot instance against the actors loaded from its module is machinery that did not exist. A per-*load* boot would need none of it, but would re-run the prologue on every selector load — and `on_load_font_bytes` does not dedup (`text/runtime/mod.rs:158-172`, straight to `dispatch_font_parse` with no resident check), so five kit loads would mean five off-thread parses of one TTF and five ids for one font.
- **`FontRef` is a wire change to kinds in use.** `DrawText` and `FontMetricsRequest` both widen, and every call site migrates.
- **Names are convention, not enforcement.** A shared `const` does not stop a caller passing a raw string, so a typo is a runtime warn rather than a compile error. This is the price of open-ended names — a game can bind `"display"` without touching the capability's schema, which a closed enum of roles would forbid.
- **A failed boot degrades rather than fails the load.** Actors instantiate and their text drops with a warn. Hard-failing every load in the module because a font did not parse is the worse outcome, but it does mean a broken baseline surfaces as missing text plus a log line, not as a load error.

### Neutral

- **Single-actor modules are untouched.** `export!(X)` keeps its bare-load target and gains nothing it must adopt.
- **Named loads are untouched.** A load carrying an export selector resolves exactly as it does today; it just also gets the module's boot.
- **No font matcher.** Selecting a face by characteristic (monospace, weight, serif) is a real system and is deliberately not built. Two baked faces have nothing to match. `Named` is a substrate a matcher could later grow on.
- **The licenses section is inert at runtime.** Nothing reads `aether.licenses` to make a decision; it exists so the bytes carry their own attribution. A tool that dumps it is worth having and is not a prerequisite.

### Follow-on work

- `aether.text` should dedup a font registration on `(namespace, name)`. The per-module-singleton boot means one module cannot double-register, but two modules baking the same face still can.
- A module-scoped state channel — a boot actor publishing an encodable value the host retains and hands to each actor's `init` synchronously — is the general answer for baseline data that has *no* owning capability to name it. It is not built here. The first boot-computed value with no capability to hold it is the signal, and by then the boot slot exists to carry it.

## Alternatives considered

- **Gate the load on its module's boot.** Await the prologue before instantiating the requested actor, so readiness is synchronous. Rejected: a load can only gate on its *own* module, and the ordering that matters is across modules — the kit's fonts against a separately-loaded panel. It buys a guarantee nobody needed and misses the one they did.
- **Query the boot actor (request/reply).** Consumers mail `Boot` and await the baseline. Rejected: `init` is synchronous and mail is not, so the reply cannot arrive during construction. Every consumer would hold an `Option` and a guard permanently — the cheapest-looking option and the one that taxes the system forever.
- **Inject boot state into each actor's `init`.** A synchronous channel does give a non-optional field and works for data no capability owns. Deferred rather than rejected — see follow-on work. It is strictly more surface than naming, and naming covers the entire motivating case.
- **Bake the fallback font into the text capability natively.** Unconditional, and it would serve non-kit components too. Rejected: it puts an opinionated typeface and its license into the substrate, and a capability that silently substitutes a font is harder to reason about than one that reports nothing was bound.
- **A closed `FontRole` enum instead of names.** Typo-proof and reviewable, at the cost of forbidding a component from binding a name the capability's schema never anticipated. Rejected in favor of shared consts, which recover most of the safety and keep the vocabulary open.
- **Have the boot actor spawn the module's consumers.** Rejected: boot runs on every load, so it would raise a console overlay for a caller who asked for the camera. Spawning is what `default` is for.
- **Keep `entry` and add `boot` beside it.** Rejected: `entry` would then be a second special slot whose name asserts something false about which type runs first.
