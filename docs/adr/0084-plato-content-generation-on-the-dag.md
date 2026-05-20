# ADR-0084: Plato — content generation realized on the DAG

- **Status:** Proposed
- **Date:** 2026-05-20

## Context

ADR-0046 committed to the content-generation pipeline *pattern* — facts → Frame → Distill → Compose → terminal observer, content-addressed handles, three-layer storage — and explicitly deferred "concrete choices about what facts exist, what framing options to support, and what render types to ship" to application-side documents that reference it as foundation. The 0.4 DAG stack provides the runtime primitives that pattern named as prerequisites: typed handles and a content-addressed handle store (ADR-0045, shipped), DAG submit/cancel/status (ADR-0047), native transforms (ADR-0048), persistent handle store (ADR-0049), and per-provider content-gen caps (ADR-0050).

**Plato** is the application layer that realizes the pattern on those primitives. This ADR records the concrete v1 realization decisions ADR-0046 left open — the fact-file format, the corpus namespace, the transform-set membership, the recipe format and compiler, the storage layers, and the provenance/triage surface. It is the north-star document the 0.5 implementation issues hang off.

Two boundaries frame every decision below:

- **Plato is a thin layer over substrate primitives, not a runtime of its own.** Facts are files on `aether.fs` (ADR-0041, shipped — `crates/aether-capabilities/src/fs.rs`); compute is native transforms (ADR-0048); dispatch is the DAG executor (ADR-0047); caching is the content-addressed handle store (ADR-0045 — `crates/aether-substrate/src/handle_store.rs`, cap `aether.handle`); provenance is the executor's own metadata. Plato owns *vocabulary and recipes*, never *runtime*.
- **Plato is public engine; the corpus it consumes is private content.** This ADR uses only the public `material.basalt.*` vocabulary from ADR-0046. Real corpora — and the application domains that motivate content generation at scale — live in separate private repositories.

One engine primitive the full pipeline needs is **not** in the 0.4 stack: a **mid-graph effectful node** (a capability call that takes input handles and produces an output handle by a round-trip), tracked in iamacoffeepot/aether#1017. ADR-0047's taxonomy is `Source` (root, effectful, no inputs) / `Transform` (mid-graph, pure) / `Observer` (terminal, effectful, no output) — which has no slot for the LLM stages *between* the first and last call (separate Distill, Translate-then-Compose). v1 therefore ships a **minimal pipeline** that fuses frame+distill into one root `Source` and makes image-gen the terminal `Observer`, and lights up the richer multi-hop pipeline when iamacoffeepot/aether#1017 lands. The realization is forward-compatible: the richer pipeline is additive, not a reshape.

## Decision

### 1. Facts are markdown-frontmatter files, one per fact, on a `corpus` namespace

A fact is a single file with YAML frontmatter (structured metadata) and a markdown body (the canonical prose), per ADR-0046 §1 and the validated spike shape (`spikes/prompt-pipeline-spike/facts/<type>/<id>.md`). **One file per fact** (`corpus://<fact_type>/<slug>.md`), not one file per material: the content-addressing cascade keys on the fact as the unit of change (ADR-0046 §2), so coupling unrelated facts into one file would desurgicalize the cascade and break per-fact reverse-walks.

Storage is a new `corpus` namespace registered in the `aether.fs` `AdapterRegistry` alongside `save`/`assets`/`config` — a `LocalFileAdapter` entry, not new cap surface. A fact becomes a typed value via two DAG nodes: a `Source` dispatching `aether.fs.read { namespace: "corpus", path }` (resolving a `ReadResult` handle), and a native `#[transform] parse_fact(ReadResult) -> Fact` that splits frontmatter from body. The `Fact` kind:

```rust
struct Fact {
    id: String,
    fact_type: String,
    properties: BTreeMap<String, String>,  // BTreeMap, not HashMap — canonical bytes need determinism
    capabilities: Vec<String>,
    tags: Vec<String>,
    body: String,
}
```

**Fact identity is two hashes, mirroring the engine's existing identity scheme.** A fact has a stable **name-hash** and a per-version **content-hash**, exactly as the substrate already splits identity (`MailboxId` is a name hash per ADR-0029; `KindId` is `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` — content-sensitive — per ADR-0030):

- **Name-hash** — `fnv1a_64(FACT_DOMAIN ++ id)` over the authored `id` string. Answers *"which fact?"*; stable across body edits; used for references, fact selection, and reverse-walks.
- **Content-hash** — over the fact's canonical bytes. Answers *"which version?"*; changes the moment the body changes. This is the value the content-addressed cascade keys on.

Both are derived at load, not authored — the file carries only the human `id` and the prose. This closes a soundness gap in ADR-0046 §2: that derivation hashes the string `fact_id`, but the string doesn't change on a body edit, and the `aether.fs.read` that loads a fact is a `Source` with an *ephemeral* handle id (ADR-0045 §3, "two fetches are two observations") — so neither the string id nor the read handle would bust a downstream cache on a content edit, and identical reads wouldn't dedup. The compiler therefore content-hashes each fact's canonical bytes and threads *that* (not the ephemeral read handle, not the bare string id) into the cascade, so a fact edit misses exactly its dependents and an unchanged fact dedups across reads and restarts. `FACT_DOMAIN` is a 16-byte constant disjoint from the kind / mailbox / handle / transform domains. (FNV-1a 64 matches the engine's first-party choice; an untrusted-corpus future would swap to a crypto hash — the same forcing function ADR-0048 names for content-addressed-by-body.) Content-addressing this way is, structurally, a one-way compositional numbering of the derivation graph — a derived artifact's id is built from its inputs' ids (ADR-0048 §4), so the id of a composed prompt encodes its entire provenance.

### 2. Three authoring layers: properties, tags, capabilities

The corpus is a graph, not a flat list (ADR-0046 open questions; validated across both spikes). Three distinct frontmatter layers, never conflated:

- **Properties** — declarative truth-claims (`substrate: ceramic`). Direct, no inference.
- **Tags** — informal search/filter keywords. No semantic load.
- **Capabilities** — derivable attributes that gate conditional-fact applicability (`chippable`, `brittle`). Populated by hand at authoring time or by LLM inference over the body **at curation time with human review — never at render time**.

The v1 mechanism is authoring discipline + this schema + conditional facts threaded as optional lens-slot context (Spike B suppressed chip-invention across all profiles this way). The *runtime* `applies_when` predicate evaluator is **convenience, not correctness** (both spikes narrowed it from "required" to "ergonomics") and is deferred behind a forcing function (a corpus large enough that per-recipe hand-listing of conditional facts is the bottleneck).

### 3. The transform set

The meta-prompting transforms split along a purity fault line:

- **Pure (native transforms, ADR-0048):** `parse_fact` (§1), `fill_lens(LensTemplate, Fact, ContextBundle) -> FilledPrompt` (template substitution; lenses are per-`(fact_type, lens_name)` files per ADR-0046 §2 and the focused-prompt discipline — never one universal prompt branching on type), and `compose(Vec<Block>) -> ComposedPrompt` (ordered concatenation; order is load-bearing; a single `Vec` input sidesteps ADR-0048's 8-parameter cap). A pure `scrub` (string-level provenance strip) is a sibling.
- **Effectful (mid-graph node, iamacoffeepot/aether#1017):** Frame's LLM-dispatch half, Distill, and the LLM form of Translate. These take an upstream handle and produce an output handle by a cap round-trip — the node the 0.4 stack lacks.

**v1 minimal pipeline (no dependency on iamacoffeepot/aether#1017):** the recipe compiler pre-fills each per-fact prompt and emits it as a root `Source` (one LLM call per fact, frame+distill fused, producing a `Block`); `compose` is a pure mid-graph transform; image-gen is the terminal `Observer`. Separate Distill, Translate-then-Compose, and any LLM-after-LLM hop activate when the effectful node lands.

### 4. Recipes and the recipe → DAG compiler

A recipe declares "these facts, through these lenses, in this order, to this observer" in TOML (the validated spike format — `[[facts]]` entries with `fact`/`lens`/`order`, an `[observer]`, `[environmentals]` as context facts). The **recipe → `DagDescriptor` compiler is a deterministic library** — orchestration is a deterministic process, not a Claude agent. Claude *authors* recipes and lenses and *triages* expensive regens; it does not perform the mechanical compile.

v1 hosts the compiler in a **CLI** (`aether-plato`) that compiles a recipe and submits via the hub RPC, because a CLI is deterministic, unit-testable against the TestBench, and scriptable. A Plato *component* that owns compilation in-substrate is a forward path (it wraps the same library) and is the seed of a cadence daemon; the harness-driver path (Claude composes the descriptor) is rejected as the primary mechanism because it puts non-deterministic orchestration in a session.

### 5. Terminal observers wire the content-gen caps

The terminal node is an ADR-0047 `Observer` whose recipient is a content-gen cap (ADR-0050): image-gen to `aether.gemini` (`aether.gemini.nanobanana.generate`), text completion to `aether.anthropic` (`aether.anthropic.cli.send`, the subscription path). Binary outputs **stage to `save://gen/<uuid>.png` and reply with a path, never inline bytes** on the wire. The image-as-input cascade (ADR-0046 §5) is **filesystem-mediated across DAG submissions**, not an in-graph edge: an observer produces no output handle, so a generated image is recovered by the *next* recipe's `aether.fs.read` Source. The terminal observer is **pluggable** — swapping the cap turns the same fact→frame→compose pipeline into a different content type (the deferred breadth in §9).

### 6. Three storage layers

Per ADR-0046 §6, separated by ownership and reproducibility:

- **Authored** (git-tracked, in the content repo) — facts, lenses, recipes, named references. Plain markdown/TOML.
- **Cache** (local-only, gitignored) — content-addressed handles + `.meta.json` sidecars; regenerable from authored + recipe; per-machine.
- **Promoted** (git-tracked, LFS for binaries) — a handle promoted to canonical, bytes + metadata copied into `references/<category>/<slug>`; one commit per promotion; the only mutable layer in git.

The substrate writes files; it is **not** a git client (ADR-0046 §10) — `git add`/`commit`/LFS happen in higher layers.

### 7. Provenance and cost-asymmetric regen triage

The DAG executor emits a `.meta.json` sidecar beside each artifact (`{ kind, handle_id, inputs, transform, transform_params, trigger, created_at }`) from its own node/edge/handle state — structured output, not human convention (ADR-0046 §7). Regen policy splits on cost (ADR-0046 §9): **cheap stages cascade automatically** (`parse_fact`/`fill_lens`/`compose`/`scrub` — mostly free native-transform recomputes), **expensive stages queue for review** (LLM and image-gen invocations surface as a machine-legible queue; a human or Claude session picks what regenerates). This is the submit-cheap-verdict / poll-expensive-execution split.

### 8. Reverse-dependency index for scale

"What depends on this fact?" is a file-scan over `.meta.json` at small scale (ADR-0046 §7) and a **rebuildable, gitignored SQLite index** at the ~10k-artifact threshold ADR-0046 anticipated. The index is **derived, never authoritative** — the file tree stays the source of truth (ADR-0046 rejected DB-as-truth); a corrupt index is nuke-and-rebuild. This is the one scale mechanism with a near-term forcing function, so it ships in 0.5.

### 9. Scope boundary for 0.5

**In 0.5:** §1–§8 — the working pipeline (minimal until iamacoffeepot/aether#1017 lands), plus the reverse-dependency index. **Deferred** (own forcing functions, not 0.5): variable-arity / salience fact selection (needs a query node returning data-dependent N — an unbuilt DAG primitive, ADR-0046 open question, and an embedding cap deliberately out of the ADR-0050 v1 scope); content types beyond image/text (each is a pluggable observer + fact type + lenses, realized as the domains demand them); and cross-session/long-horizon snapshot persistence (depends on ADR-0049 plus application-runtime semantics that live outside this repo).

## Consequences

**Positive:**
- Content generation collapses from bespoke per-pipeline binaries into recipes over shared substrate primitives — one vocabulary, one provenance format, one triage surface.
- Iteration is cheap for the bulk (native-transform cascades dedup for free) and deliberate for the paid LLM/image calls (the review queue), matching the real cost asymmetry.
- The pluggable terminal observer means a new content type is "a new observer + fact type + lenses," not a new orchestrator.
- The realization is forward-compatible: the richer multi-hop pipeline lights up additively when iamacoffeepot/aether#1017 lands; nothing in v1 is thrown away.

**Negative:**
- v1's pipeline is the fused-minimal shape until the mid-graph effectful node ships; recipes that need separate Distill or LLM-Translate wait on iamacoffeepot/aether#1017.
- Facts-as-files commits reverse-walks to scan-or-index (not SQL joins); the index is the mitigation but adds a rebuild discipline.
- Per-machine cache means iteration on an un-promoted fact doesn't share work across machines (acceptable — that's a single-machine workflow).

**Neutral:**
- The corpus, recipe library, and concrete fact-type / render-type catalog live in the content repo, not here. This ADR commits to *mechanism*, not to any application's content.
- Plato adds the `aether-plato` CLI crate and a Plato types crate (the `Fact` / `LensTemplate` / `Block` / `ComposedPrompt` kinds + the native transforms); it adds no new substrate runtime.

## Alternatives considered

- **Monolithic Plato component owning facts, compilation, dispatch, and storage internally.** Rejected — re-grows the caching/parking/dispatch the 0.4 substrate already provides; Plato stays a thin layer.
- **All-in-database corpus + cache.** Rejected per ADR-0046 — loses git diffs and human/LLM editability; the file tree + rebuildable index gets query speed without giving up either.
- **Harness-driver compilation (Claude builds the descriptor each session).** Rejected as the primary path — puts non-deterministic orchestration in a session; the deterministic compiler library is the deliverable, hosted in a CLI (and later a component).
- **One universal framing prompt branching on fact type.** Rejected — silent quality degradation when a type is added; per-`(fact_type, lens_name)` files make "adding a type" a forcing function, which is a feature.
- **Wait for the mid-graph effectful node before shipping any Plato.** Rejected — the minimal fused pipeline is genuinely useful and the richer pipeline is additive; coupling the whole release to one engine issue is unnecessary.

## Follow-up work

- **Engine prerequisite (0.4):** iamacoffeepot/aether#1017 — mid-graph effectful node. The richer pipeline (§3) depends on it; the minimal pipeline does not.
- **Engine prerequisites (0.4, in flight):** the DAG stack — ADR-0047 (issues 973–977), ADR-0048 (issues 978–982), ADR-0049 (issues 983–988), ADR-0050 (issue 989).
- **0.5 implementation cluster:** the issues filed against the "aether 0.5" project board realizing §1–§8 — corpus + `Fact` kind + `parse_fact`; the property/capability/tag schema; the `corpus` namespace + file layout; `fill_lens`; `compose`; the recipe format + compiler CLI; terminal observers + `save://gen` staging; provenance sidecars + the triage queue; the reverse-dependency index.
- **Deferred, future ADRs / releases:** variable-arity fact selection (with the query-node and embedding-cap prerequisites); the breadth of non-image content types; long-horizon snapshot persistence.
