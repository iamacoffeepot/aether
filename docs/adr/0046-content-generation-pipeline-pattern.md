# ADR-0046: Content-generation pipeline pattern

- **Status:** Proposed
- **Date:** 2026-04-24

## Context

Aether-substrate-as-content-gen-daemon is a stated direction (substrate hosts long-running pipelines that generate text + images for downstream consumption, with Claude-in-MCP-harness for review and iteration). LLM-driven content-generation workflows in this domain — prompt composition for image models, structured-output drafting for narrative content, multi-stage pipelines with scoring and refinement — share a recognizable shape across applications: load source material, transform through one or more LLM stages, dispatch to a final model, capture provenance, surface results for review. Without a unifying primitive, each new pipeline shape (object reference sheets, character portraits, environment studies, narrative beats, planning artifacts) tends toward a bespoke orchestrator with its own composition logic, validation, dispatch, and triage scaffolding.

The unifying pattern beneath these workflows has a specific shape worth committing to as an architectural pattern, separate from the engine primitives it builds on. Five forces drive it:

- **Cost asymmetry across pipeline stages.** Text-side operations (LLM distillation, translation, composition, scrubbing) are ~$0.001-0.05 per call; image-side operations (Gemini, DALL-E, etc.) are ~$0.05-0.30 per call and 10-100× slower. A one-fact-changed cascade through a typical pipeline triggers 5-20 cheap regens but only 1-3 expensive ones. Architectures that treat all operations uniformly waste the asymmetry.
- **Outputs feed back as inputs.** Generated images become reference handles for downstream renders (object-sheet → scene; portrait → environmental study). The pipeline isn't a DAG-per-render; it's a persistent computation graph spanning weeks of work, where this morning's render references last week's promoted assets.
- **Provenance is load-bearing.** "What facts went into this image" + "what depends on this fact" are real questions the user asks during iteration. Every published artifact needs an audit trail.
- **Iteration is the dominant cost.** Authors don't generate once; they regenerate as facts evolve, framings shift, and reference sheets get promoted. A naïve regenerate-everything approach is wasteful; a content-addressed cache with selective propagation makes iteration tractable.
- **Authored content vs. derived content split.** Source-of-truth facts are git-tracked authored prose. Derived blocks, composed prompts, and generated images are all reproducible from the authored layer + recipe — they should not pollute the version-controlled history except at promotion points.

This ADR commits to the architectural pattern that addresses these forces. It builds on engine primitives ADR-0045 (typed handles, computation DAG) and forthcoming engine work for the DAG submit/cancel/status vocabulary, content-addressed handle persistence, and an LLM sink. It does not commit to any specific application of the pattern — concrete choices about what facts exist, what framing options to support, and what render types to ship live in application-side documents and code that reference this ADR as foundation.

The pattern was validated through two spikes (Spike A: text-only frame / distill / compose pipeline across multiple model tiers and LOD configurations; Spike B: image generation through the same pipeline via Gemini). Architectural claims that survived first contact with real workloads are captured as commitments below; claims that didn't survive or didn't get exercised are listed explicitly as open questions rather than buried as "implementation details."

## Decision

### 1. Facts as primary artifacts

The pipeline's source of truth is a corpus of **facts** — atomic, canonically-authored statements about the world being generated. Each fact is a markdown file with YAML frontmatter:

```markdown
---
id: material.basalt.density
type: material-property
property: density
material: basalt
value: 2.9
unit: g/cm³
tags: [stone, columnar, weight]
---

Basalt is a dark, fine-grained volcanic rock. Density 2.9 g/cm³ —
heavier than concrete (2.4) but lighter than iron. Columnar form
exhibits natural hexagonal jointing that makes individual columns
easier to wedge free than to break across.
```

Frontmatter holds structured metadata (id, type, properties, tags); body holds the canonical prose statement. Facts are git-tracked, authored manually or by curated LLM proposals, never derived. The fact corpus is the only artifact class whose content cannot be reproduced from anywhere else in the system.

### 2. Frame + Distill as composable transforms

A fact alone is not a prompt fragment. Two transforms reshape facts into prompt-suitable blocks:

- **Frame** maps a fact through a *lens* — an authored markdown artifact, organized per fact type (`lenses/<fact_type>/<lens_name>.md`), declaring its prompt template and any context slots (other facts that condition the rendering). Same source fact, different angle of approach depending on the lens and the context filling its slots. `AsFact` is verbatim pass-through (no LLM call); other lenses dispatch through an LLM with the template filled in.
- **Distill** maps a framed fact to a target level of detail. LOD compresses the prose to a budget (terse / brief / full / exhaustive), via an LLM call sized appropriately (Haiku for cheap LODs, Sonnet for nuanced ones).

Pipeline shape per fact:

```
Fact ─→ Frame { lens, [context_facts...] } ─→ FramedFact ─→ Distill { lod, model } ─→ Block
```

Each transform produces a handle. Handles are content-addressed:

- `framed_id = hash(fact_id, lens_id, lens_template_hash, sorted_context_handle_ids, model)`
- `block_id = hash(framed_id, lod, model)`

A change to the source fact misses both caches. A change to the lens template or to a context fact misses only Frame for renderings using that lens or context. A change to LOD or model misses only Distill, and only at that LOD (other LODs stay cached). The cascade is precise.

### 3. Composition is ordered concatenation

Blocks compose into a prompt via an explicitly-ordered transform. Order is load-bearing because most image-generation models exhibit decaying token weight from front to back: blocks placed earlier in the composed prompt influence the output more strongly. The composer accepts `Vec<Ref<Block>>` (variable-arity, ordered) and produces `Handle<ComposedPrompt>`. How the order is decided (recipe enumeration, priority-sorted query, salience ranking) is upstream and orthogonal to the composer; the composer just respects the order it's given.

A separate **Translate** transform may apply selectively per block (e.g., lighting blocks anchored in Japanese for image-gen models that respond to it) before composition. Same content-addressing rules as Frame and Distill: `translated_id = hash(block_id, target_language)`.

### 4. Image generation as a terminal observer with parallel input channels

Image generation is the pipeline's terminal node — it takes two distinct input channels:
- A `Ref<ComposedPrompt>` — the text prompt
- A `Vec<Ref<Image>>` — reference images for style/subject conditioning

These are parallel inputs, not text-composed. Image-gen models accept references as separate API parts (Gemini's `inlineData`, etc.), so the substrate dispatches the prompt + the resolved reference image bytes as a single request. Output is a `Handle<Image>` cached identically to other handles.

### 5. Image-as-input cascade

Generated images are first-class handles in the computation graph, not terminal-only artifacts. A scene render's reference list may include the promoted object-reference sheets generated by earlier renders. This makes the system a **persistent computation graph**, not a DAG-per-render — handles published from yesterday's render are referenced by today's, and the dependency chain spans the entire history of generation.

This is what promotes content-addressed handles + persistent handle store from "caching optimization" to "load-bearing for the basic use case." Without persistence across DAG submissions, image-as-input is unworkable.

References do more than preserve subject identity. Spike B Phase 2 surfaced two adjacent properties worth committing to:

- **References propagate style coherence to new content** in the same render. A teacup added beside a celadon-teapot reference inherits the celadon glaze without any fact, recipe entry, or prompt text declaring color. The reference channel carries properties the corpus may not have authored.
- **References are constraint surfaces, not just identity sheets.** Spike B Phase 4 (no-reference render) produced more out-of-scope inventions than the same recipe with a reference (Phase 2). The reference's role isn't purely aesthetic — it absorbs adjacent-prior pull and reduces the grading surface to fact-driven dimensions only. This reframes the production workflow: a bare-corpus render is best understood as a stress test for fact coverage; the reference-anchored render is the production path.

### 6. Three-layer storage

Three storage classes, separated by who owns the state and what's reproducible:

- **Authored layer (git-tracked, in the application's content repo)** — facts, lenses (per-fact-type prompt templates), recipes, named references (canonical-handle pointers), shared composition templates. All plain markdown / JSON. Conventional git workflow: individual file commits with messages.
- **Cache layer (local-only, gitignored)** — content-addressed file tree at `~/.cache/aether/handles/{blocks,images}/<hash[:2]>/<hash[2:]>.{json,png}`. Each artifact has a sidecar `<hash>.meta.json` written by the DAG at regen time, recording inputs, transform, params, and trigger. Regenerable from the authored layer + recipe; nuke and rebuild on demand. Per-machine.
- **Promoted layer (git-tracked, with LFS for large binaries)** — when a handle is promoted to canonical, its bytes + metadata are copied from cache into `references/<category>/<slug>.{png,json,meta.json}`. One commit per promotion. The promotion file is the only mutable layer in git; updating it doesn't propagate to existing references. Following Spike B Phase 4's references-as-constraint-surfaces finding, promoted references are double-duty: identity carriers (preserved across renders) AND constraint absorbers (reducing the surface where adjacent-prior contamination can manifest). Recipes that anchor against promoted references benefit from both.

The split keeps the version-controlled history small and readable (only authored content + canonical promotions), while the cache absorbs the volume of speculative iteration.

### 7. DAG records the "why"

Provenance is structured output from the DAG executor, not a human-authored convention. At regen time, the DAG knows: which inputs feed this node, which transform produced it, which trigger initiated this run, when it ran. That metadata is written alongside the artifact:

```json
{
  "kind": "image",
  "handle_id": "abc123...",
  "inputs": ["fact:material.basalt.density:hash...", "block:...:hash...", ...],
  "transform": "image-gen",
  "transform_params": {"model": "gemini-3.1-flash-image", "aspect_ratio": "9:16"},
  "trigger": "fact:material.basalt.density bytes changed",
  "created_at": "2026-04-24T12:34:56Z"
}
```

Reverse walks ("what depends on fact X?") become file-scan or grep against `.meta.json` files. Slow at scale but acceptable for triage UX; SQLite as a rebuildable local index is a forward-compatible addition when scale demands.

### 8. Selective propagation (git-snapshot model)

When a fact changes, content-addressing produces a new handle id for downstream artifacts; the old handles remain valid and remain referenced by any artifact that previously consumed them. Promotion to a canonical name pins a specific handle id, not a moving target. Updating the canonical pointer doesn't invalidate existing references; it only affects future generations that resolve the name freshly.

This is closer to git's snapshot/tag model than to salsa's auto-recompute. The system reports staleness (which artifacts reference a now-superseded fact version) but does not act on it without explicit instruction. Humans (or a Claude-in-MCP-harness session triaging the queue) decide what to regenerate.

### 9. Cost-asymmetric regen policy

Cheap regenerations cascade automatically:
- Frame, Distill, Translate, Compose, Scrub all rerun on every cache miss without confirmation
- Cumulative cost per fact-change cascade: a few cents and a few seconds

Expensive regenerations queue for review:
- Image generation invocations are surfaced as a queue ("12 images would change; here are the prompt diffs and impact context") rather than auto-running
- A human or Claude session picks what to actually regen

This gets most of the "see everything update as we change stuff" UX feeling for the cheap stuff (which is the bulk of the work), while keeping image-gen on explicit decision.

### 10. Substrate is a content producer, not a git client

The substrate writes to the filesystem. Git operations — `git add`, `git commit`, LFS push, repo cloning — happen at higher layers: CLI utilities, Claude-via-MCP using the Bash tool, or human-typed commands. The substrate does not depend on `git2-rs` / libgit2 and does not hold credentials, signing keys, or repo configuration. The DAG-records-the-why metadata is structured data the higher layer can pipe into a commit message body when committing, but the runtime does not author commits itself.

## Open questions

- **Fact selection mechanism.** Three plausible: explicit reference (recipes name fact IDs), tag/predicate query (recipes describe filters, system returns matching set), or salience-ranked retrieval (recipes describe render intent, embeddings + top-k pull most-relevant facts). The first works with current ADR-0045 vocabulary; the latter two introduce variable-arity DAG outputs not yet designed.
- **Variable-arity DAG outputs.** A query node returns N handles where N depends on data. ADR-0045's current `Edge { from, to, slot }` assumes fixed slots. Fan-out over runtime-determined N needs new edge or node vocabulary, scoped to the engine ADR that ships query support.
- **Branch nodes for conditionals.** Static conditionals ("if region X, include block Y") resolve at recipe-evaluation time and produce a fully-resolved DAG that doesn't need branching primitives. Dynamic conditionals (output-dependent: "if scoring rejects candidate, run refinement pass"; "if first generation misses must-have features, regenerate with a stricter directive") need predicate edges. Both are common in image-generation workflows that combine parallel candidate selection with reference-conditioned refinement. Likely a separate engine ADR.
- **Lens artifact details still to pin down.** The lens-as-per-type-artifact shape is committed in the body, but several mechanics remain open: (1) **Slot validation semantics** — required vs optional slots, what happens when an optional slot is unfilled (template `{{#if SLOT}}` directive vs pre-substitution null collapse), how strictly to enforce slot fact-type matching at recipe load. (2) **Multi-lens-per-fact** — a fact may want to be expressed simultaneously through multiple lenses (instruction + feeling) producing multiple blocks for the composer; recipe shape needs to express this without becoming verbose. (3) **Model declaration location** — lens declares the model (lens author owns cost-tier choice) vs recipe override (per-render cost optimization). (4) **Lens template hash boundary** — hash whole file (frontmatter included) vs body only (separates declarative metadata edits from prompt edits). (5) **Defaults resolution order** — render-type defaults vs fact-type defaults vs recipe overrides; which wins when they conflict.
- **Relationships and conditional realities.** A fact's appearance often depends on context external to the fact: "basalt near water" reads differently from "basalt in cliff face." Two architectural shapes worth comparing once stress-tested: *scene-context as positional/relational metadata* (the scene declares where things are; lens templates have slots for adjacent-bodies / regional-position), and *conditional sub-clauses inside the fact body* (a fact has a baseline plus optional `## When near water` / `## When quarried as piling` sections that scene context activates). The simplest v1 fallback is "describe all realities in the fact body and let the lens LLM pick what's relevant for the scene context" — costs nothing to author, surfaces drift the LLM mixes incompatible conditionals where structured activation would help.
- **LOD as integer vs labeled levels.** `lod: 3` is opaque; `lod: terse | brief | full | exhaustive` carries meaning. Likely labels-with-numeric-mapping — humans recipe-author with labels, the cache key uses the integer.
- **LLM nondeterminism in cached transforms.** Same `(fact, lens, lod, model)` invocation can produce slightly different outputs across calls. First-call-wins-with-manual-reroll is the current sketch; whether this is right under heavy iteration (where authors might want to regenerate variance and pick) is open.
- **Bootstrap case.** First-generation reference images are generated without any prior reference handles. This is just "ref list can be empty" but the recipe authoring flow + the propagation rules need to handle it explicitly.
- **Recipe file format.** TOML / JSON / markdown-with-frontmatter — implementation detail subordinate to the structure of recipe entries (fact id + frame + lod + ordering).
- **Promotion workflow specifics.** What command/component performs the cache → promoted-layer copy? How is the corresponding git commit produced? Whether to ship a CLI helper or rely on Claude-via-MCP composing the steps from io + Bash is open.
- **Fact-grounded grading and the fact-coverage graph.** Once images are rendered, fact-grounded grading checks the rendered output against the facts that fed it. Three buckets, not two: (a) *violations* — facts the image fails to render correctly; (b) *conditional-dimension gaps* — attribute dimensions activated by stated facts but unspecified in the recipe (e.g. `material.glazed-ceramic` activates chip-status as a live dimension; absence of any `condition.*` fact means the model fills it in either way); (c) *out-of-scope inventions* — attributes that don't follow from any stated or derivable fact (decorative scrollwork on a fact that says "no decorative relief"). The first is a model failure; the second is corpus-completeness signal that suggests authoring a new conditional fact; the third is bias-defense work. Distinguishing (b) from (c) requires reasoning over fact properties to determine what's "in scope," which presupposes the conditional-facts apparatus below. Spike B further narrows: violations split into *recipe-fixable* (can be addressed by changing facts, lenses, prompts) and *model-capability-bounded* (model can't reliably render the asked-for thing — e.g. dust motes in air); the grading framing should distinguish these because the remediation paths differ. Reference-carried attributes need explicit prompt instruction to be tagged correctly (else the grader confuses reference-derived properties with out-of-scope inventions).
- **Conditional facts and the property / capability / tag layer.** A fact's applicability often depends on properties of other facts (chip-status applies only when the material is chippable). This needs three distinct authoring layers: declarative *properties* (frontmatter fields like `substrate: ceramic`), informal *tags* (search-y keywords), and derivable *capabilities* (`chippable`, `paintable`, `brittle` — the binding for `applies_when` predicates on conditional facts). Capabilities are populated either manually at material-fact authoring time or by LLM inference over the material body with human review at curation time, not at render time. Authoring shape, predicate language, derivation discipline, and how the runtime evaluates `applies_when` predicates are all open. Probably warrants its own ADR when conditional-fact authoring becomes a concrete need.
- **Model bias vs. observer perception.** An attribute appearing in a rendered image but not in any fact can be (a) genuine perception worth turning into a conditional fact, (b) training-distribution prior leaking through (e.g. artisan-teapot training images bias toward chips regardless of prompt), or (c) cultural / linguistic anchor (a model's concept may be denser in one language register than another — concretely surfaced by Gemini's solo-Koto-only-in-Japanese behavior). Distinguishing these requires cross-model and cross-language probing — the model variation matrix and selective translation tests are the diagnostic instruments. The conditional-facts apparatus reduces ambiguity by grounding "what's reasonable to infer" in stated material capabilities; attributes consistent with capabilities are defensible inferences, attributes orthogonal to capabilities are suspected artifacts.
- **Adjacent-prior contamination.** Surfaced empirically in Spike B Phase 4: introducing a new fact can pull in adjacent training-distribution priors that erode unrelated facts. Example — a `glaze-cobalt-blue` fact pulled the rendered teapot toward "vintage cobalt pottery" priors strong enough to introduce worn handle spots that violated `condition.flawless`, a constraint that survived every prior render. Three remediation candidates: (1) *anti-prior framing in fact bodies* (the cobalt fact body explicitly disclaims antique / weathered / vintage associations), (2) *stronger directive language on competing facts* (`condition.flawless` body becomes more emphatic), (3) *reference-anchoring as the production default* (bare-corpus renders treated as stress tests for fact coverage; reference-anchored renders treated as the production path because references absorb adjacent-prior pull). Option 1 is authoring-discipline (free, scales with corpus); option 3 reframes the references-vs-facts division of labor.

## Engine prerequisites

The pattern depends on engine primitives that are partially shipped and partially in flight:

- **ADR-0045 Phase 1** (shipped): `Handle<K>`, `Ref<K>` wire type, handle store, handle-aware mail dispatch.
- **ADR-0047** (proposed, Phase 2): DAG submit/cancel/status vocabulary, descriptor validation, executor for sources + observers.
- **ADR-0048** (proposed, Phase 3): `#[transform]` macro, `aether.dag.transforms` custom section, wasmtime `Func::call` integration, content-addressed transform handle ids.
- **ADR-0049** (proposed): persistent handle store across substrate restart. Depends on ADR-0048's content-addressing — without it, restored handle ids wouldn't match recomputed ones. ADR-0046's image-as-input cascade is the headline forcing function.
- **ADR-0050** (proposed): LLM completion sink — `aether.llm.complete` request kind + reply kind, adapter registry (subprocess + HTTP), parallel to ADR-0041 (io) and ADR-0043 (net).
- **Branch node vocabulary** (forthcoming engine ADR if needed): predicate edges or guard nodes to express dynamic conditionals.

This ADR's body assumes those prerequisites exist or will exist. Each is the right scope for its own ADR review and cannot be folded into this strategy doc without bloating it past usefulness.

## Spike A validation

A standalone text-only spike at `spikes/prompt-pipeline-spike/` exercised the Decision body's architectural claims end-to-end against a small authored fact corpus (a utah teapot on a wooden surface, with material/surface/observer/environmental facts and per-fact-type lens templates) using `claude -p` subprocess invocation as the LLM transport. Five experiment runs were logged in `spikes/prompt-pipeline-spike/RUNS.md`: initial vertical slice, observer differentiation, model variation matrix (Haiku / mixed / Sonnet / Opus / mixed-Opus), conditional-fact validation, distill stage layered on, and a controlled distill-model comparison holding the frame model constant.

Architectural claims that empirically held:

- **Per-type focused lens prompts produce coherent, differentiated output.** Declarative-path lenses (`object/rendering-instruction.md`) maintained the "no mood, no observer voice" directive across all model tiers including Haiku. Perception-path lenses (`material/feeling.md`, `surface/feeling.md`) carried observer voice through every sentence rather than appending it as a closing flourish. Per-(fact_type, lens_name) authoring discipline validated.
- **Observer-as-synthesizer differentiates output across observer profiles.** Same source bytes, same lighting, same lens, same model — different observer = unmistakably different prose register. Validated against `quiet-domestic` and `young-fascinated` observer pairs. Observer voice survived across all three model tiers (Haiku / Sonnet / Opus).
- **Cache addressing is surgical at every transform stage.** Swapping the observer invalidated only perception-path blocks; declarative-path blocks stayed cached. Switching distill model invalidated only the distill output, not its frame input. Same `(fact, lens, context, model)` tuple produced byte-identical cache hits regardless of which profile triggered the call. Content-addressing delivers what the body promises.
- **Conditional facts work as a corpus-completeness mechanism without the runtime `applies_when` apparatus.** Authoring `condition.flawless` and threading it as an optional context slot into the material and object lenses suppressed chip invention across all five profile variants. The chip — initially read as model bias — turned out to be defensible inference filling an unmapped attribute dimension; closing the dimension via a manually-authored conditional fact shut it out cleanly. The runtime predicate is convenience (auto-include conditional facts when their predicate is satisfied), not correctness for v1.
- **Distill compresses Frame outputs by 50-65% while preserving observer voice and propagated constraints.** Wholeness affirmation (sourced from `condition.flawless` via the lens slot) survived through Distill in every tier. Specific phrasings carried verbatim from frame to distill output where they served the brief LOD. Voice preservation was strong enough that distilled output reads as the same voice writing tighter, not a different voice paraphrasing.
- **Cross-model probing diagnoses bias vs. genuine perception.** When all models produced the same chip-shaped invention, that's a fact-coverage gap. When models diverged (Haiku: chip near spout; Sonnet: chip near foot; Opus: condensation bead instead) — that's particularization variance, and Opus's contextually-grounded choice (condensation in morning light) showed cross-model variation is diagnostic, not noise.

Recommendations that emerged:

- **Haiku for distill, Sonnet/Opus for frame.** The controlled experiment isolated stage-specific model effects: Haiku-distill compresses 50-62% on identical input, Sonnet-distill 39-52%, Opus-distill 32-41%. Distill is mechanical "compress while preserving X" work that doesn't reward higher-capability models; frame benefits from nuance. Use the cost saving from cheap distill to fund quality on frame.
- **Same-model frame+distill is a wasteful pairing.** Sonnet-frame → Sonnet-distill produced minimal real compression (the operation is near-identity). Meaningful compression requires a capability gap between the model that produced the input and the one compressing it. The cost-optimized default profile pattern is *higher-tier frame, lower-tier distill*.
- **Profile shape `default | by_stage | by_lens`.** Authored as YAML; resolution priority `by_lens > by_stage > default`. Two profile axes (per-stage, per-lens) cover the practical override space without ballooning the configuration surface.

Open questions narrowed by the spike:

- **`applies_when` runtime apparatus** moves from "required for v1" to "convenience for ergonomics." The corpus-completeness work is authoring conditional facts; the runtime evaluation is just auto-include vs. recipe-level explicit inclusion.
- **LLM nondeterminism in cached transforms** gains an empirical anchor: at the distill stage specifically, model choice produces *systematic* compression-ratio differences (50-62% Haiku vs. 32-41% Opus on identical input), not noise. The cache key correctly treats different models as different cache entries.

Open questions left untouched:

- The image-as-input cascade (Decision §5) was not exercised — Spike A is text-only by design. Spike B is the next milestone for that.
- Variable-arity DAG outputs (open-question item 2) and branch nodes (item 3) didn't surface in the spike because the recipe enumerates fact IDs explicitly. Both will likely surface during Spike B's grading-and-refinement loop.
- Promotion workflow specifics, recipe file format ergonomics, and the named-reference mechanism are still open.

The spike is preserved as a worked-example reference under `spikes/prompt-pipeline-spike/`. Cargo workspace-isolated (matches the existing `aether-mail-spike-host` pattern), gitignored cache layer, full RUNS.md lab notebook. Future spike B work (image gen + grading + refinement) can build on it or branch from it.

## Spike B validation

Building on Spike A's text-only scaffold, Spike B added image generation, image-as-input cascade, multimodal fact-grounded grading, and fact-coverage workflow validation — all dispatched against Gemini's Nano Banana 2 (`gemini-3.1-flash-image-preview`) for image gen and `gemini-3-pro-preview` for grading. Four phases logged in `RUNS.md`.

Architectural claims that empirically held:

- **Pipeline composes end-to-end through to pixels.** Frame → Distill → Compose → Generate ran as a single recipe-driven invocation. The composed prompt feeding Gemini is unchanged from what Spike A produced. ADR-0046 §1-§4 validated against image gen with no shape change in upstream stages.
- **Bytes-through-cache works at PNG-tier sizes.** 600-700 KB PNGs cache cleanly via the existing content-addressing scheme. Memory pressure is negligible at this scale; the worry that "transforms shouldn't process binary blobs" was overstated for sub-MB outputs (multi-MB outputs would still want the wrap-in-sink pattern).
- **Conditional-fact propagation reaches pixels.** `condition.flawless` survived frame → distill → compose → image generation; rendered teapots show intact glaze with no chips/cracks. End-to-end conditional-fact propagation is real, not just text-to-text.
- **Image-as-input cascade preserves identity AND propagates style.** Phase 2 used a Phase 1 teapot render as a reference for a teapot+cup scene. The teapot's exact silhouette, glaze color (undeclared in the corpus), wood-table grain pattern, kitchen background, and lighting direction all carried through. The newly-added teacup adopted the celadon glaze from the reference without any color fact specifying it.
- **Multimodal grading produces structured, actionable signal.** The three-bucket framing (violations / conditional-dimension gaps / out-of-scope inventions) survived first contact and produced distinct content in each bucket. Markdown-section output was sufficient for parsing — JSON schema can wait.
- **Capability-layer inference happens at grade time.** The grader correctly identified `unglazed-clay-color (made live by material.glazed-ceramic)` and similar capability-derived dimensions from fact bodies alone, with no upfront `capabilities: [...]` frontmatter. The authored `capabilities` layer remains optional for v1; LLM-side derivation works.
- **Fact-coverage workflow closes the loop.** Phase 4: a previously-unmapped color dimension was authored as `aesthetic.glaze-cobalt-blue` and threaded through lens slots. The new render produced cobalt blue, overriding the model's celadon prior. Authoring → re-render → grading-confirms-gap-closed cycles correctly.

Recommendations and open-question narrowings:

- **Markdown-section grading output is sufficient for v1.** Each bucket is a clearly-delimited markdown section, easy to parse downstream. Defer structured JSON schema until a downstream consumer demands it.
- **Reference-carried disambiguation requires explicit prompt instruction.** The grader needs to be told "if an attribute could plausibly come from a reference image, it's NOT out-of-scope, it's reference-carried" — without that line, color and similar reference-derived properties get flagged as inventions, and the bucket loses meaning.
- **Three-bucket framing extends to a fourth diagnostic distinction.** Within violations: *recipe-fixable* (change facts/lenses/prompts) vs *model-capability-bounded* (the model can't reliably render the requested attribute — Spike B's dust-motes case). The grading rubric and remediation triage UX should distinguish these.
- **Fact authoring discipline gains a new pattern.** Adjacent-prior contamination (Phase 4 cobalt → vintage pottery → wear) suggests fact bodies for distinctive properties should include explicit anti-prior framing — disclaim the most common adjacent associations the model would otherwise pull in.

Open questions narrowed:

- **`applies_when` runtime apparatus** (carried over from Spike A) further narrowed: capabilities can be derived at *grade time* by the vision LLM rather than authored at fact-curation time, reducing the runtime predicate's strict v1 necessity. The authored layer still benefits from explicit capabilities for fact-selection efficiency once that mechanism ships.
- **LOD-as-labels-vs-integer**: confirmed labels (validated in the LOD-slider experiment of Spike A's late phase).
- **Bootstrap case** (open-question item 7) confirmed trivial: omit `[[generate.references]]` from the recipe; the rest of the pipeline runs unchanged.

Open questions surfaced or sharpened:

- **Adjacent-prior contamination** is now an authoring-discipline open question. Spike B Phase 4 surfaced it; the three remediation candidates (anti-prior framing, stronger competing-fact directive, reference-anchoring as default) are listed in the open-questions section.
- **Production-vs-stress-test render distinction.** Bare-corpus renders (no reference) are the right shape for testing fact coverage; reference-anchored renders are the right shape for production output. The recipe library should make this distinction clear (perhaps via separate recipe categories or a `[bare]` / `[anchored]` annotation).

The spike artifact under `spikes/prompt-pipeline-spike/` now includes the image-gen + grading + cascade code; gitignored `cache/`, `out/`, and `.env.local`. RUNS.md carries the full lab notebook through Phase 4. Future image-gen spikes (e.g., 3D model generation cascade) can extend it further.

## Consequences

**Positive:**
- One vocabulary across content-gen workflows. Bespoke per-pipeline binaries collapse into substrate components running typed DAGs against shared sinks.
- Provenance is automatic. Every artifact carries the structured metadata of how it was produced, no convention required.
- Iteration is fast for cheap regens, deliberate for expensive ones — matches the actual cost asymmetry of paid LLM/image-model APIs.
- Cross-machine work is supported via the promoted layer (canonical artifacts in git+LFS); per-machine cache absorbs speculative iteration.
- The pattern extends naturally beyond prompt generation. NPC dialogue, faction moves, season arcs, environmental storytelling — same fact-frame-distill-compose shape, different terminal observer.

**Negative:**
- Spike validation covered the cheap text path and a small image-gen smoke test, not production-scale workloads. Edge cases in recipe ergonomics, framing-set lock-in, and large-cascade behaviour will still surface during full asset-pipeline use.
- Per-machine cache means iteration on a fact whose new version isn't promoted yet doesn't share work across machines. Acceptable because that's exactly the workflow where you'd be on one machine anyway.
- Reverse walks via file-scan are slow at scale. Triage UX may need a SQLite index after ~10k cached artifacts; the index is rebuildable from the file tree so this is a forward-compatible addition.
- Multi-step LLM-driven workflows (parallel candidates → score → conditional refinement) are not directly expressible without dynamic-conditional vocabulary. Pipelines that need this shape today have to compose it externally (a driver process spawning Claude sessions or a bespoke orchestrator); an Aether-native equivalent needs branch nodes (deferred to engine ADR).
- Selective propagation puts the burden of "what should I regenerate" on the human / triaging Claude session. Auto-recompute is forbiddable by cost; the staleness queue is the substitute, but it's still triage work.

**Neutral:**
- The application repo holds the fact corpus, recipe library, and concrete framing / render-type catalog. This ADR does not commit to any application-level choices; those live in application-side documents that reference this one.

## Alternatives considered

- **Bespoke per-pipeline binaries (current state).** Each new pipeline shape gets its own orchestrator binary with its own subagent driver, prompt composition, and result triage. Works, but every pipeline reinvents the orchestration. Rejected for unifying purposes.
- **All-in-database storage (SQLite + LFS).** Single storage system, atomic transactions, fast queries, cross-machine sync via committed DB. Rejected because LFS-tracked SQLite blobs lose meaningful diffs and merge ergonomics; the file-tree alternative gets cross-machine sync, atomic-enough writes (content-addressed = write-once), AND visible diffs at the cost of slower reverse-walks. SQLite remains available as a rebuildable local index.
- **Auto-recompute on fact change (salsa-style).** When a fact changes, every downstream artifact regenerates automatically. Rejected because image regen is too expensive to auto-run; the cost-asymmetric policy gets the UX feel of "see it update" for cheap stuff while preserving cost-discipline for expensive stuff.
- **Single bigger ADR covering the engine primitives + the strategy.** Rejected because the engine primitives are wire/runtime concerns reviewed by engine contributors; the strategy is an architectural pattern reviewed by anyone building pipelines on Aether. Different audiences, different review cycles, different rates of change.

## Follow-up work

- Engine ADRs for DAG vocabulary, handle persistence, LLM sink, and (if needed) branch nodes — referenced above. Each scoped tightly enough for focused review.
- A worked-example component crate demonstrating the pattern end-to-end against a small public fact corpus. Real implementation will surface ergonomic issues this paper design can't catch.
- Application-side documentation (in the consuming repo) listing concrete framings, fact-type taxonomy, render-type catalog, and recipe-library structure. Not part of public Aether.
