# ADR-0028: Component-embedded kind manifest

- **Status:** Proposed
- **Date:** 2026-04-19

## Context

When a runtime-loaded component introduces new mail kinds, the substrate has to register those kinds *before* the wasm boots so the component's `resolve_kind("name")` calls during init succeed. ADR-0010 shipped this via `aether.control.load_component`'s `kinds: Vec<LoadKind>` field — the loader (Claude or another driver) hand-authors a JSON description of each new kind's name, encoding shape, and field layout, and the substrate registers from the mail payload.

That shape was built before any concrete component exercised the path. `aether-hello-component` only uses substrate-built-in kinds (Tick, DrawTriangle, Ping/Pong — registered at boot from `aether_kinds::descriptors::all()`) so the `kinds` field was always empty. The `aether-demo-sokoban` component (PR #122) was the first real consumer, and it exposed three compounding problems:

1. **The JSON is hand-duplicated from the Rust types.** `#[repr(C)] struct SokobanMove { direction: u8, _pad: [u8; 3] }` tells the compiler the exact wire layout; the loader then hand-writes a 60-line JSON blob re-describing that layout with the names "direction" and "_pad" and primitive tags "U8" and `array_len: 3`. If the struct changes, the JSON has to change too — nothing enforces the match.

2. **Struct padding is silently the loader's problem.** `#[repr(C)]` inserts padding bytes between mixed-size fields (`u8` next to `u32` gives 3 bytes of silent padding). Rust's compiler handles this automatically for the component's own memory, but the `LoadKind` JSON has to declare padding explicitly or the substrate reads garbage. Nothing warns on mismatch.

3. **`LoadKind`'s encoding vocabulary is deliberately flat and therefore limited.** `LoadKindEncoding` admits only `Signal` (empty) or `Pod { fields: Vec<LoadKindField> }` (fixed-size scalar + array fields). No strings, no `Vec<T>`, no nested structs, no enums. This drove the 16×16 sokoban grid cap: `SokobanState.cells` had to be a `[u8; 256]` fixed array rather than a `Vec<u8>`, because `Vec<u8>` can't be expressed via `LoadKind`. The full `aether_hub_protocol::SchemaType` vocabulary supports every shape Rust components actually want; `LoadKind` is a stripped-down subset to keep the hand-authored JSON simple.

All three problems have the same root cause: **the loader shouldn't be in the business of describing kinds at all.** The information lives in the Rust type definitions inside the component. The loader is just a human or agent manually retyping what the compiler already knew.

Two shapes considered for moving the source of truth onto the component side:

**(a) Component self-registers during init.** Add a host fn `register_kind_self(name, schema_bytes)` that the guest's `Component` init shim calls from the `KindList::resolve_all` walker before each `resolve_kind`. Substrate registers on-the-fly.

Downsides: conflict detection happens mid-init rather than pre-instantiation — if a component declares a kind that conflicts with an existing one, the substrate has to trap the wasm and clean up a partially-allocated mailbox. Kind vocabulary advertisement to the hub (per ADR-0027-ish timing) has to move from post-load to post-init. Opaque to external tooling: you can't inspect a component's kinds without running it.

**(b) Component embeds kinds as wasm binary metadata.** The derive macro writes kind descriptors into a WebAssembly custom section at compile time. The substrate reads the section before instantiation, validates against its registry, registers on success, then boots the wasm exactly as it does today.

WASM custom sections (section id 0, arbitrary bytes with a name) are the spec's explicit extension point for user-defined metadata — the mechanism DWARF debug info, the `producers` section, linking info, and source maps all use. Runtimes are required to ignore them, so nothing executes them; any wasm parser can walk them. `wasmtime::Module::custom_sections(name)` returns them directly, so the substrate's load path gets them for free before it ever instantiates.

(b) preserves every property of the current design — pre-flight conflict detection, clean early failure, post-load kind announcement — while moving the source of truth to the component. It is also inspectable without a wasm runtime: `wasm-tools dump foo.wasm` will print the section, so debugging "what kinds does this component declare?" is a command-line operation.

This ADR commits to (b).

Separately, the derive emission strategy matters. A component's `type Kinds` typelist (ADR-0027) is only the *receive* side; kinds the component emits as replies or to sinks are declared via `#[derive(Kind)]` on the type but don't appear in the typelist. Rather than grow a second typelist ("IntroducedKinds"), `#[derive(Kind)]` itself emits the section entry: every derive expands to the existing `impl Kind` plus a `#[used] #[link_section = "aether.kinds.v1"]` static carrying the kind's postcard-encoded descriptor. When Rust's linker builds the wasm, every such static concatenates into the section automatically. The component author declares kinds exactly once, via `#[derive(Kind)]`, and the manifest is a side-effect of the declaration. `export!` stays focused on init/receive/drop wiring.

The `#[used]` attribute prevents dead-code elimination from stripping unused static sections, which means a component that statically links `aether-kinds` but only uses `Tick` still carries schemas for `Key`, `MouseMove`, etc. in its binary — roughly 50–200 bytes of postcard per kind × ~20 built-in kinds = 2–4KB of bloat per component. Accepted. The invariant "every `#[derive(Kind)]` type reachable from the binary has its schema in the manifest" is clean and worth the bounded overhead; DCE-sensitive emission would be fragile (a kind used only by name in a typelist-macro expansion might be stripped and silently disappear from the manifest).

## Decision

**Components declare their kind vocabulary by embedding a wasm custom section that the substrate reads before instantiation. The `kinds` field on `aether.control.load_component` is removed.**

### Custom section format

- **Name**: `aether.kinds` (stable; versioning lives inside the payload — see below).
- **Payload**: a concatenation of records. Each record is `[version: u8] [postcard(KindDescriptorVN)]` where `VN` corresponds to the version byte. A record carries a kind's name and its full `SchemaType` tree — the same descriptor the substrate advertises via `describe_kinds`.

The full `SchemaType` vocabulary (strings, `Vec<T>`, `Option<T>`, nested structs, enums with any field shape) is available to runtime-registered kinds. `LoadKindEncoding`'s flat `Signal | Pod` restriction — and the component-side workarounds it forced — is retired.

Because each derive emits its own `#[link_section]` static, the section is formed by the linker's natural concatenation of multiple statics with the same section name. The parser reads records sequentially: read one version byte, decode the following bytes with the matching version's parser, advance, repeat until the section ends. Postcard is self-delimiting per record once the version-specific struct is chosen.

### Versioning policy

In-payload per-record versioning — stable section name, first byte of every record identifies the record's format version. The convention matches how most WebAssembly custom sections handle evolution (DWARF, `linking`, `name`) rather than the less-common name-suffix approach.

Implications:

- **v1 is the starting version.** The byte is `0x01`. A future change to `KindDescriptor`'s shape (new field, restructured `SchemaType` variant, etc.) gets a new version number. v1 and v2 records can coexist within a single binary because each carries its own tag.
- **The substrate carries a decoder per known version.** Parsing is `match version { 1 => decode_v1(...).lift_to_canonical(), 2 => decode_v2(...), _ => Err }`. The `lift_to_canonical` step maps an older record into the substrate's current canonical shape, filling defaults for anything the old version didn't express. The registry/routing code stays agnostic of wire version — the compat surface is localized to one lifter per retired version.
- **Unknown version = reject the load.** An old substrate reading a newer component (or any version the current build doesn't know) responds with `LoadResult::Err` naming the unknown version. Skipping unknown records is explicitly not an option: a missing kind surfaces much later as `resolve_kind → KIND_NOT_FOUND`, and the resulting init-time panic is painful to diagnose. Clean early failure beats lazy cryptic failure.
- **Retirement of old versions is deliberate.** Pre-1.0 we remove a version's decoder as soon as no in-tree component emits it. Post-1.0 we keep decoders for a published deprecation window; the specific window is a decision we'll make at 1.0, not now.
- **Expected cadence.** `SchemaType` is already comprehensive, so version bumps should be rare. Each new version costs roughly a decoder (~30–80 lines) plus a `lift_to_canonical` mapping. Carrying many versions is bounded in code size, not an ongoing burden.

### Derive responsibilities

`#[derive(Kind)]` expands to:

1. The existing `impl ::aether_mail::Kind for T` (name, IS_INPUT per ADR-0021 follow-up).
2. A new `#[used] #[link_section = "aether.kinds"]` static whose value is `[0x01] [postcard(KindDescriptor { name: T::NAME.into(), schema: <T as Schema>::schema() })]` — one record, version byte first.

The static is const-constructed at compile time. Because postcard produces a known size for a concrete schema, the static's length can be computed at macro expansion. If that proves awkward, the derive falls back to `#[used] static` of a typed record struct that postcard can serialize inline — same wire effect.

The `Schema` derive is already feature-gated on `descriptors` (hub-protocol dep); emitting the section static is gated on the same feature. Components built for wasm targets enable the feature explicitly. Components that never introduce kinds (e.g. a hypothetical future `#[derive(Kind)]` on a type not emitted anywhere) simply have an unused `impl Kind` and an unused section static — the static costs bytes in the binary, nothing else.

### Substrate load path

1. Hub sends `aether.control.load_component` with `wasm: Vec<u8>` and `name: Option<String>`. No `kinds` field.
2. Substrate parses the wasm binary (via `wasmtime::Module::new` followed by `Module::custom_sections("aether.kinds")`) to extract the manifest bytes.
3. Substrate decodes the manifest by reading records sequentially: one version byte, then the version-specific decoder, then the next record. Each decoded record lifts to the current canonical `KindDescriptor` shape.
4. Substrate cross-references each descriptor against its registry:
   - Kind unregistered: register it with `register_kind_with_descriptor`.
   - Kind registered with matching schema: no-op (idempotent; aether-kinds kinds that got linked into the binary also show up here).
   - Kind registered with mismatched schema: abort load with `LoadResult::Err` naming the offending kind. Matches today's pre-flight conflict detection.
5. Substrate allocates the mailbox, instantiates the wasm, runs init.
6. Substrate's `announce_kinds` fires post-load, exactly as today.

A component without the manifest section (old components, hand-written WAT used in tests) is treated as declaring zero kinds — same as today's empty `LoadKind` list. Built-in kinds resolve as always. This preserves the test suite and gives a natural migration path for any out-of-tree consumers.

### Wire changes

- `aether-kinds`: the `control_plane::LoadKind`, `LoadKindEncoding`, `LoadKindField`, `LoadKindPrimitive` types are removed. `LoadComponent` and `ReplaceComponent` lose their `kinds: Vec<LoadKind>` field.
- `aether-hub`: the `load_component` / `replace_component` MCP tools lose their `kinds` argument. The tools pass through `binary_path` and `name` only; the substrate reads kinds from the wasm directly.
- `aether-hub-protocol`: no change (this type system is already what components will embed).
- `aether-mail-derive`: `#[derive(Kind)]` grows the section-emitting behavior when the `descriptors` feature is active.

## Consequences

- **Zero-authoring load.** An agent's tool call becomes `load_component(binary_path)`. The hand-authored JSON, along with all its mismatch failure modes, is gone.
- **Schema richness matches `#[derive(Kind)]`.** Strings, `Vec<T>`, `Option<T>`, nested structs, enums — whatever the derive already supports for native kinds also works for runtime-registered kinds. The sokoban demo's 16×16 cap can be relaxed whenever we want (follow-up).
- **Inspectable offline.** `wasm-tools dump my-component.wasm` prints the section; tools can audit a binary's kind vocabulary without loading it.
- **Binary bloat, bounded.** `#[used]` costs ~2–4KB per component in schemas from aether-kinds that may not be used. Measurable; acceptable.
- **Breaking wire change.** `LoadComponent` and `ReplaceComponent` change shape; `load_component` / `replace_component` tools change signature. Pre-1.0 project; explicitly allowed. The sokoban demo and any external caller must update. Backward-compat shims are not worth writing — there is exactly one in-tree user.
- **Derive now runs at kind definition sites even in wasm-only builds.** A kind struct in a `#[no_std]` cdylib that derives Kind now emits a section static too. Needs `const`-fn postcard serialization or a stable workaround. If that's infeasible in `const` context, fall back to lazy initialization in a Rust `ctor`-style path — wasm has no ctors, so pre-init is the only option. We'll prove the const path works or, if not, encode descriptors at build time (`build.rs` outputs the bytes; the derive includes them via `include_bytes!`).
- **`wasm-tools` becomes part of the implicit dev tooling story.** Not a hard dep, but the clear recommended way to inspect a component. Worth a note in `CLAUDE.md`'s tooling section.

## Alternatives considered

- **(a) Component self-registers via host fn during init.** Rejected: moves conflict detection to mid-init (ugly rollback), loses offline inspectability, opaque to external tooling, requires trapping wasm to report a conflict cleanly.
- **Keep `LoadKind` but make the derive emit it as a Rust helper.** Rejected: only helps Rust-side loaders; agents still hand-author JSON. Half a solution.
- **Hub parses the wasm section instead of the substrate.** Rejected: the hub doesn't use `wasmtime`; adding a wasm parser dep to the hub just to do what the substrate can do for free is misplaced responsibility. The substrate already owns wasm validation and instantiation.
- **Use an exported wasm function (not a custom section) that returns manifest bytes.** Rejected: requires instantiation to read. The whole point is to keep conflict detection pre-instantiation. Also not inspectable by static tooling.
- **Drop `#[used]` and rely on DCE to trim the section.** Rejected above: a kind referenced only by name in a typelist macro might get stripped, silently disappearing from the manifest. Bounded bloat is a better trade than fragile emission.
- **Version by section name (`aether.kinds.v1` / `aether.kinds.v2`).** Rejected in favor of in-payload versioning: the dominant convention for mature WebAssembly custom sections (DWARF, `linking`, `name`) is a stable section name with internal version fields. A stable name also means tooling (`wasm-tools dump`, future editor integrations) doesn't have to know about every live version to find the section. In-payload versioning composes more naturally with the per-record structure we already have from `#[link_section]` concatenation.
