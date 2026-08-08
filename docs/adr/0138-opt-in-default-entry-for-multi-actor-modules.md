# ADR-0138: Opt-in Default Entry for Multi-Actor Modules

- **Status:** Accepted (shipped — opt-in default entry for multi-actor modules in `crates/aether-component/src/component/runtime/load.rs`, #2736)
- **Date:** 2026-07-07

## Context

ADR-0096 §3 gave every multi-actor module an implicit default entry: the export selector defaults "to a designated entry type when omitted," and that designated type is the first one listed in `export!`. A bare `load` with no export selector instantiates index 0.

That default is baked in at three layers with no opt-out:

- **The macro.** `export!(A, B, C)` promotes `A` to the entry unconditionally (`aether-actor/src/wasm/mod.rs`, the multi-actor arm forwards `$first` as `@entry`).
- **The wasm binary.** The `aether.namespace` custom section is written from the entry type's `NAMESPACE`, and the 3-arg `init` shim constructs the entry type when the load carries no type tag.
- **The host.** The component load path takes `actors.first()` when no export is named, with no branch for "this module has no default."

For a subsystem library — a crate that ships a grab-bag of unrelated actors the harness loads independently (the reference kit packs a camera, a mesh viewer, a world mesher, a mover, and a widget set) — "the one I happened to list first" is a meaningless thing to hand a bare load. The order is an authoring accident, not a designation. A caller who omits the export selector against such a module has almost certainly made a mistake, and the current contract answers that mistake by silently instantiating whichever actor sits at index 0.

The immediate trigger is retiring the kit's locomotion actor, which currently holds the first slot. Promoting a sibling into that slot would preserve a default that never should have been implicit — it relocates the arbitrariness rather than removing it.

## Decision

A multi-actor module has **no default entry unless it opts in**.

- `export!(A, B, C)` designates **no** entry. A bare `load` of that module with no export selector is a hard error that names the available exports, rather than instantiating one by list position.
- A module that wants a default names it: `export!(entry = A, B, C)` makes `A` the bare-load target. The remaining types are exported and selectable by name exactly as before.
- `export!(X)` — the single-actor module — is unchanged. One exported type is an unambiguous bare-load target, so it stays the target; the opt-in requirement applies only where "which one" is a real choice.

Mechanically:

- The entry-less multi-actor form **omits the `aether.namespace` custom section** and emits a **section-level no-entry marker**, so the host can distinguish a defaultless module from a legacy single-actor module and from a multi-actor module loaded without a selector.
- The host load path returns `LoadResult::Err` — naming the module's exports — when a bare, defaultless load arrives with no export selector. `aether-mcp` mirrors this in its entry-namespace resolution and replica-name derivation.
- The export selector itself (ADR-0096 §3), the actor-type tag carrier (ADR-0090), and the per-type `aether.kinds.inputs` manifest (ADR-0033) are unchanged. This ADR changes only what a *missing* selector resolves to.

This supersedes the "defaulting to a designated entry type when omitted" clause of ADR-0096 §3. The rest of §3 — the optional export selector and per-type introspection — stands.

## Consequences

### Positive

- **No meaningless defaults.** A library module stops answering an omitted selector with an authoring accident. The caller either names an export or gets an error that lists them.
- **A default entry is a stated intent.** `entry = A` is a designation a reader can see and a reviewer can question, not a side effect of list order.
- **Bare-load mistakes surface.** An omitted selector against a defaultless module fails loudly with the exports named, instead of loading the wrong actor and failing later or subtly.

### Negative

- **The two existing multi-actor sites migrate.** The reference kit and the test-fixtures bundle both rely on first-position bare-load today; each either opts into `entry = …` to preserve behavior or drops the default deliberately.
- **The wasm section contract grows a case.** The no-entry marker and the conditional `aether.namespace` emission are new state on the module, mirrored by a new branch in the host load path and the `aether-mcp` resolution. Contained to `aether-actor` / `aether-actor-derive`, `aether-capabilities` (the component load path), and `aether-mcp`.

### Neutral

- **Single-actor modules are untouched.** The overwhelming majority of components (`export!(X)`) keep bare-load behavior.
- **Named loads are untouched.** Any load that already carries an export selector resolves exactly as before, defaultless module or not.

## Alternatives considered

- **Keep implicit first-is-entry, promote a sibling in the kit.** Rejected: it preserves an implicit default that is arbitrary for a library module and pushes the same footgun onto the next reader.
- **Bare load of a defaultless module silently loads nothing (no-op).** Rejected: a silent no-op hides the caller's mistake; a hard error naming the exports is actionable.
- **Make the entry always explicit, breaking single-actor bare-load too.** Rejected: a single-actor module has exactly one unambiguous target and thousands of call sites; forcing a selector there is churn with no ambiguity to resolve.

## Related

- ADR-0096 — Multi-actor wasm modules. This ADR supersedes the default-entry clause of its §3; the export-selector mechanism and per-type manifest stand.
- ADR-0090 — Init-config byte carrier with the leading actor-type tag; unchanged, still carries the selected type at load.
- ADR-0033 — Handler-driven inputs manifest; the per-type export list it surfaces is what a defaultless load's error names.
- ADR-0024 — Dual-target `_p32` shims; the entry-constructing `init` shim gains the no-entry case.
