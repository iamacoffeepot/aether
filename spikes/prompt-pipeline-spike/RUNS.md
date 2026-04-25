# Run log

Lab-notebook record of experiments. Append a new entry per run-of-interest.
Format is loose on purpose; the goal is continuity for future work, not
formal benchmarking.

Entries record: date, recipe + variations, model assignment, wall-clock,
qualitative observations on the output, and a one-line verdict on what was
validated or invalidated.

The composed prompt outputs themselves live in `cache/blocks/` (gitignored,
content-addressed by their input hash). They are reproducible from the
authored layer + recipe + model choice.

---

## 2026-04-24 — initial vertical slice, end-to-end

**Recipe:** `recipes/teapot-on-table.toml`
**Observer:** `observer.quiet-domestic`
**Environmentals:** `lighting.window-morning`
**Models (lens-declared):**
- `object/rendering-instruction.md` → Haiku
- `material/feeling.md` → Sonnet
- `surface/feeling.md` → Sonnet

**Wall-clock:**
- Cold run: ~81s (3 sequential `claude -p` invocations)
- Cached re-run: 4ms (0 API calls)

**Observations:**
- Pipeline ran end-to-end without errors. Three Frame calls, content
  composed in declared order with `Camera:` and `Aspect ratio:` lines
  prefixed.
- The declarative path (object) produced clean rendering instructions —
  form, parts, light behavior, no mood. The lens prompt's `do not
  describe mood, atmosphere, or observer perspective` directive held.
- The perception path (material, surface) produced output where the
  observer's voice ("the way old things hold their habits", "this is
  not sad", "the way a threshold is familiar") is present in every
  sentence, not appended at the end.
- Cache surgical: re-run with no input changes hits cache for all three
  blocks.
- Sonnet ignored the 3-4 sentence length cap. Perception blocks were
  ~150-200 words each, ~3-4× the requested length.

**Verdict:**
- ✅ Architecture is structurally sound (per-type lenses, observer-driven
  perception path, content-addressed cache).
- ✅ Observer voice differentiation works in single-call simplified form.
- ⚠ Length discipline isn't enforced by Sonnet alone. Either tighten the
  lens prompt (stronger word cap, examples), or rely on a Distill stage
  to compress to LOD.

**Next:** Add a second observer profile, run the same recipe, compare
side-by-side. The teapot block hits cache (no observer in its key);
only the two perception blocks regenerate. Confirms observer-as-
synthesizer actually shifts output.

---

## 2026-04-24 — observer differentiation test

**Recipes:** `recipes/teapot-on-table.toml` (quiet-domestic) and
`recipes/teapot-on-table-young.toml` (young-fascinated)
**Environmentals + lenses + models:** unchanged from prior run
**Wall-clock for the second recipe:** 36s (2 claude calls, not 3)

**Cache behavior validated:**
- Teapot block (`object.teapot` via `object.rendering-instruction`) hit
  cache — no observer in its cache key, no regen needed when observer
  swapped.
- Material + surface blocks missed cache (different observer in key)
  and regenerated.
- Stderr confirmed only 2 of 3 transforms invoked claude.

**Observer voice differentiation (qualitative):**

| | quiet-domestic | young-fascinated |
| --- | --- | --- |
| ceramic register | settled, affectionate, reverent of mundane ("old things hold their habits", "this is not sad… a kind of dignity") | investigative, building a model, tentative metaphor-making ("a cool that doesn't give ground", "you're not sure yet which ones") |
| surface register | inhabitant prose ("the way a threshold is familiar… without ceremony and without neglect") | second-person observation ("it occurs to her that the center's relative cleanness isn't care so much as it is the natural geometry of reaching") |
| sentence cadence | longer, contemplative, present in the room | precise + slightly tentative, building a model from data |

The voice differences are pronounced enough to be unmistakable when read
side by side. Same source bytes, same lighting, same lens, same model —
different observer = different prose register.

**Finding — chip incident reframed as fact-coverage signal:**
The young-fascinated observer rendered "the chip near the foot." The
source material fact mentions chips as a property of glazed ceramic
("chips reveal the unglazed body underneath"); the observer
particularized this into a specific located chip. Initially read as
hallucination, this is better understood as defensible inference:
glazed-ceramic IS chippable (the material body says so), so chip-
status is a live attribute dimension. The observer filling that
dimension with "chipped near foot" is reasonable in the absence of
a fact that says otherwise.

The deeper insight: the chip surfaces an *unmapped attribute
dimension*, not a violation. The recipe doesn't specify whether the
teapot is whole or chipped. The system should treat this as a
fact-coverage gap — a candidate for a conditional `condition.flawless`
fact gated on chippable materials. Authoring `condition.flawless`
and adding it to the recipe would close the dimension; without it,
the LLM fills the space with a defensible interpretation.

**Verdict:**
- ✅ Observer-as-synthesizer differentiates output in the way the
  architecture predicted. Worth committing to as a primitive.
- ✅ Cache surgery: changing observer invalidates only perception-path
  blocks; declarative-path blocks stay cached. Surgical.
- ✅ Observer creative interpretation isn't a bug — it's signal that
  the fact corpus has unmapped dimensions for the materials in scope.
  The productive response is *conditional facts gated by material
  capabilities*, not *constraining the observer*.

**Architecture refinements surfaced (added to ADR-0046 open questions):**
- Fact-grounded grading is a triple operation: violations + conditional-
  dimension gaps + out-of-scope inventions.
- Conditional facts need a property / capability / tag distinction —
  capabilities (derivable, gate applicability) are the binding for
  `applies_when` predicates, distinct from declared properties and
  informal tags.
- Distinguishing genuine signal from model-distribution bias requires
  cross-model + cross-language probing (the model matrix experiment
  is the diagnostic instrument).

**Next:** Model variation matrix. Add CLI override for model assignment
per transform type, run the matrix the user proposed (all-Haiku /
all-Sonnet / Haiku+Sonnet / all-Opus / Opus+Sonnet) against the same
recipe + observer, compare the resulting prompts side by side.

---

## 2026-04-24 — model variation matrix

**Recipe:** `recipes/teapot-on-table.toml` (quiet-domestic observer)
**Profiles run:** `mixed-haiku-sonnet`, `all-haiku`, `all-sonnet`,
`all-opus`, `opus-sonnet`
**Cache:** cleared before this run (lens-id format change invalidated
all prior entries)
**Total wall-clock:** ~114s for 5 profile runs (9 distinct claude calls
after content-addressed dedup; opus-sonnet hit cache on every transform
because all-opus and all-sonnet had populated everything it needed).

**Initial bug found:** profile `by_lens` keys didn't match because lens
frontmatter ids had a redundant `lens.` prefix while profiles used
recipe-style ids. Fixed by dropping the prefix from lens frontmatter
ids; lens id now matches recipe references exactly.

**Cache addressing validated:** runs that share `(model, lens, fact,
context, observer)` produce identical output via cache hit, regardless
of which profile triggered the call. `mixed-haiku-sonnet` ≡ `all-sonnet`
for material+surface (both use sonnet for those lenses); `opus-sonnet`
≡ `all-opus` for material+surface (both opus). Confirms the cache key
shape is right.

**Observer voice survives across model tiers.** All three pure-model
profiles (Haiku / Sonnet / Opus) produced output recognizable as the
quiet-domestic observer. Different prose registers, same personality
underneath:
- Haiku: simpler, more direct, first-person
- Sonnet: more polished, second-person ("you"), sharper observational pivots
- Opus: most figurative, varies grammatical person ("she"/"my"/"her")

**Sensory invention is model-specific, NOT a universal training-data prior.**
For the material/feeling block, the chip prediction differs across models:
- Haiku: chip near the **spout**, first-person observer
- Sonnet: chip near the **foot**, second-person observer
- Opus: **no chip**; chose a condensation bead "from last night's rinse"
  instead — a transient feature better grounded in the morning-light
  context

This empirically resolves the bias-vs-perception question I raised
earlier. The chip isn't an unavoidable training-distribution pull;
Opus reasoned to a different particularization. Haiku and Sonnet
default to the most directly-cued attribute (the source mentions
chips); Opus chose a more contextually grounded one (condensation in
morning light).

**Cross-model consistency on the chip location** is also revealing:
Haiku says "near the spout," Sonnet says "near the foot." Both invented
a location because the recipe doesn't pin one. Same fact-coverage gap
expressed differently. A `condition.flawless` fact (or its inverse)
would close this dimension across all three models.

**Object block discipline holds across all models.** None of the three
added observer voice or mood to the rendering-instruction lens output.
The declarative lens prompt's "do not describe mood, atmosphere, or
observer perspective" directive held even at Haiku tier. Per-type
focused prompts validated.

**Length cap still ignored** at every tier. Haiku produces shorter
sentences but the same 4-5 sentence count as Sonnet and Opus. Length
discipline isn't a model-quality issue; it's a prompt-engineering
issue at the lens level (or needs a Distill stage).

**Verdict:**
- ✅ Profile-driven model overrides work after the lens-id format fix.
- ✅ Cache addressing surgical across profiles. Same key = same output.
- ✅ Observer voice differentiation preserved across all model tiers —
  the architecture is not model-fragile.
- ✅ Each model contributes meaningfully different prose register —
  per-tier voice is real signal worth choosing for.
- ✅ The "chip" earlier interpreted as possible bias is empirically
  model-specific (Haiku/Sonnet only; Opus diverges). Bias defense via
  cross-model probing works as a diagnostic.
- ⚠ When invented attributes vary by model (chip-spout vs chip-foot vs
  no-chip), cross-run consistency requires either committing to one
  model OR fact-grounding via conditional facts (`condition.flawless`
  etc.). This is the fact-coverage-graph workflow we'd flagged.

**Recommendations from this matrix:**
- For *creative coherence within a single render*: Opus on perception
  lenses produces the most contextually-grounded sensory inferences
  (the condensation bead vs. arbitrary chip placement).
- For *cross-render consistency*: any model is fine if the fact corpus
  closes the conditional dimensions. Without that, each model invents
  its own particulars.
- For *cost-efficient mixed*: `mixed-haiku-sonnet` (Haiku for declarative,
  Sonnet for perception) produces good output at lower cost than
  `all-opus` and is the natural default for non-critical renders.

**Next:** Two paths.
1. Author a `condition.flawless` conditional fact, add it to the recipe,
   re-run the matrix — does the chip disappear across all three models?
   Validates the conditional-facts story without needing the runtime
   `applies_when` predicate yet (just author the fact and add it).
2. Layer in Distill — pipe each Frame output through a Distill call with
   target LOD, see if the verbose perception blocks compress without
   losing observer voice. Tightens the composed prompt toward image-gen-
   suitable length.

---

## 2026-04-24 — conditional-fact validation (chip suppression test)

**Hypothesis:** authoring a `condition.flawless` fact, threading it as a
context slot into both `material.feeling` and `object.rendering-instruction`,
and including it in the recipe will suppress chip invention across all
three model tiers — without needing the runtime `applies_when` predicate
apparatus.

**Changes:**
- New fact `facts/condition/flawless.md` with body explicitly forbidding
  chips/cracks/damage but allowing non-structural wear.
- `material/feeling.md` lens gained an optional `condition` slot and a
  `## Object condition` section in its template; explicit guidance to
  honor the stated condition added to the lens body.
- `object/rendering-instruction.md` lens gained the same optional slot.
- Recipe `teapot-on-table.toml` declares `condition = "condition.flawless"`
  in environmentals.
- `frame.rs` updated to walk lens-declared slots (so optional unfilled
  slots get replaced with empty strings instead of leaving raw
  `{{NAME}}` placeholders in the prompt).

**Wall-clock:** ~81s for 5 profile runs. Surface lens unchanged so all
surface blocks hit cache; only material + object regenerated under the
new lens templates.

**Result: chip suppressed across every profile.**

| Profile | Material block — chip mention | Wholeness affirmation |
| --- | --- | --- |
| all-haiku  | no | "the glaze fused smooth and whole… where nothing has been disturbed or worn through" |
| mixed-haiku-sonnet | no | "The surface is unbroken end to end, the glaze continuous… in that wholeness, not grand, just quietly right" |
| all-sonnet | no | (same as mixed-haiku-sonnet — same model+lens+inputs, same cache key) |
| opus-sonnet | no | "It looks the way it has looked a thousand mornings… kept and used and entirely intact" |
| all-opus   | no | (same as opus-sonnet) |

**Notable findings:**

1. **All three models honored the condition fact** — not just by skipping
   chips, but by actively expressing wholeness in the observer voice.
   Each tier did this in its characteristic register:
   - Haiku: direct, concrete affirmation ("the glaze fused smooth and whole")
   - Sonnet: aphoristic ("in that wholeness, not grand, just quietly right")
   - Opus: contextually-grounded ("kept and used and entirely intact",
     directly mirroring "carefully kept" from the fact body)

2. **The nuance in the condition fact carried through.** The fact body
   says "Signs of use that don't compromise structure are still fair game".
   Sonnet's surface block parsed this correctly: "the wood has taken on
   a slightly deeper tone, **not damage so much as the surface remembering
   what it holds**." The model distinguished "wear" from "damage."

3. **Observer voice preserved.** quiet-domestic still reads as quiet-
   domestic — affectionate, unhurried, attentive. The condition fact
   added a constraint without flattening the personality.

4. **Cache invalidation was surgical.** Adding a slot to two lens
   templates invalidated only those lenses' cache entries; surface
   stayed cached. Same recipe re-run with the same lens templates would
   hit cache for everything. This is the architectural property we
   wanted: edit a lens, re-render only what depends on it.

**Verdict:**
- ✅ Conditional facts work as a corpus-completeness mechanism without
  needing the runtime `applies_when` apparatus. Threading the condition
  fact as a context slot is sufficient to constrain LLM invention.
- ✅ The fact-coverage workflow validates: chip was an unmapped
  attribute dimension; authoring `condition.flawless` and including it
  closed the dimension; chip disappeared.
- ✅ All three model tiers respect the constraint. No model bypassed
  the condition; lower tiers (Haiku) were as compliant as higher
  (Opus).
- ✅ The architecture's design promises (surgical cache, observer
  voice preservation, per-type lens discipline) all held under this
  evolution. Adding a new fact type + threading it through lenses
  was a 5-minute author-side change with no runtime work needed.

**Implication for ADR-0046:** the conditional-facts open question can
narrow significantly. The runtime `applies_when` predicate apparatus
isn't strictly required for v1 — manual authoring of conditional facts
+ recipe-level inclusion + lens-level slot threading is sufficient to
exercise the pattern. The runtime predicate becomes a *convenience*
(automatic inclusion of conditional facts when their predicate is
satisfied) rather than a *correctness requirement*.

**Next:** Layer in Distill — pipe each Frame output through a Distill
call with a target LOD, test whether the verbose perception blocks
compress without losing the observer voice. Tightens the composed
prompt toward image-gen-suitable length, and exercises the second
transform type in the architecture.

---

## 2026-04-24 — distill stage layered on (perception lenses only)

**Hypothesis:** a per-(fact_type, lod) Distill stage applied to the
verbose perception blocks will compress them to image-gen-suitable
length (~2-3 sentences each) while preserving observer voice and
the conditional-fact (wholeness) constraint that propagated through
Frame.

**Changes:**
- New module `src/distills.rs` (template loader) + `src/transforms/distill.rs`
  (Distill transform). Distill takes a Frame output, looks up a per-
  (fact_type, lod) template at `distill/<fact_type>/<lod>.md`, fills
  `{{INPUT}}`, calls claude. Cache key includes the framed input bytes,
  template hash, and model.
- Recipe `FactEntry` gained an optional `lod: Option<String>` field.
  When set, Frame output is distilled before composition; when unset,
  Frame output passes straight through.
- New templates `distill/material/brief.md` and `distill/surface/brief.md`.
  Both target 2-3 sentences, default to Haiku model, with explicit
  preserve/drop directives (preserve observer voice + sensory anchors;
  drop self-referential rumination + redundant metaphors).
- Recipe updated: material and surface entries now have `lod = "brief"`.
  Object entry left without lod (rendering-instruction output is already
  short).

**Wall-clock for matrix re-run:** ~72s for 5 profiles. 6 new claude
calls (3 distill model variants × 2 perception lenses); the rest hit
cache.

**Composed prompt length comparison:**

| Profile | Without distill | With distill (`brief`) | Compression |
| --- | --- | --- | --- |
| all-haiku | ~500 words | 217 words | ~57% |
| mixed-haiku-sonnet | ~500 words | 220 words | ~56% |
| all-opus | ~530 words | 280 words | ~47% |

All composed prompts now fall comfortably in image-gen-suitable
length (under 300 words).

**Voice preservation observations:**

- The distilled outputs are clearly the *same voice* as the frame
  outputs, not a different voice paraphrasing. The directive "the
  result must read as the same voice writing a tighter version, not
  a different voice paraphrasing" held across all three model tiers.
- Specific phrasings carried through verbatim:
  - Sonnet kept "a thing that has been here and has been kept"
  - Opus kept "Kept and used and entirely intact"
  - Sonnet kept "absorbing rather than performing" for the surface
- Each model preserved different highlights from its source frame
  output — reasonable lossy choices, not arbitrary cuts.

**Wholeness affirmation preserved through distill.** All distilled
material blocks still affirm intact-ness ("Intact glaze throughout",
"unbroken end to end", "kept and used and entirely intact"). The
condition fact's effect propagated through both Frame and Distill —
the second transform didn't lose the constraint the first one had
internalized.

**Cache surgery validated again.** Distill-stage cache misses were
exactly 6 (the new combinations); profiles with overlapping model
choices for distill hit cache (all-sonnet shared cache with mixed-
haiku-sonnet for distill since both default to sonnet for distill;
opus-sonnet shared cache with all-opus for the same reason on
material+surface distill).

**Aside — observed model parity in distillation.** Distillation seems
less model-quality-sensitive than perception framing. Haiku-distilled
output is roughly comparable in quality to Sonnet- and Opus-distilled
output for this task. Distillation is a more mechanical operation
(compress while preserving X) than perception rendering (synthesize a
voice through inputs). Suggests cost-optimization recommendation:
*Haiku is fine for distill across the board; reserve Sonnet/Opus
spend for the perception path where their nuance shows up.*

**Verdict:**
- ✅ Distill compresses Frame outputs by ~50-60% while preserving
  observer voice and downstream constraints (wholeness).
- ✅ Per-(fact_type, lod) template structure works. Each template is
  focused on the fact type it operates on; same per-type-prompts
  principle as lenses.
- ✅ Cache key composition surgical for distill (framed input bytes
  + template hash + model). Independent of upstream Frame stage's
  cache state.
- ✅ Pipeline is now end-to-end: Frame → Distill → Compose, with
  optional Distill per recipe entry.
- ✅ Composed prompts are now of practical length for image-gen
  dispatch in Spike B.

**State of the spike:** Spike A's text-only pipeline is essentially
complete and validates the core architectural claims:
- Per-type focused lens prompts produce coherent, differentiated output
- Observer-as-synthesizer differentiates perception across observers
- Conditional facts (manually authored + threaded through lenses) close
  unmapped attribute dimensions
- Cache addressing is surgical at every transform stage
- Model variation produces meaningful register differences but
  preserves architectural properties
- Distill compresses without losing voice or constraints

**Next options:**
1. **Decompose perception lens to proper Impose-parallel + Synthesize.**
   Currently the perception lens is one rich call with everything inline.
   The full architecture splits into parallel `Impose<fact, modifier>`
   calls + a synthesizing `Synthesize<observer, fact, perspectives>`
   call. Validates the caching-reuse story when modifiers vary.
2. **Add more environmentals** (e.g., `weather/clear`, `time/early-morning`)
   to stress the parallel-impose path with multiple modifiers.
3. **Spike B: image gen + grading + refinement.** Layer Gemini on top
   of the spike's composed prompts; add fact-grounded grading via vision
   LLM; build the refinement loop. Substantive next step.
4. **Pause the spike, write up findings.** Update ADR-0046 with the
   empirical evidence; capture the spike as a worked-example reference;
   move on to the engine work the architecture depends on (DAG vocabulary
   ADR, persistent handles ADR, LLM sink ADR).

---

## 2026-04-24 — controlled distill model comparison

**Setup:** profile schema gained a `by_stage` override so frame and
distill models can be set independently. Three controlled profiles
authored, all with `frame: sonnet` held constant; distill varies
across `{haiku, sonnet, opus}`. Same recipe (`teapot-on-table.toml`).
This isolates distill-model effects from frame-model effects, which
the earlier matrix had confounded.

**Wall-clock:** ~43s for 3 profile runs (4 new claude calls; sonnet-
distill of sonnet-frame was already cached from prior matrix runs).

**Result table (compression on identical input bytes):**

| Distill | Material wc | Surface wc | Material compression | Surface compression |
| --- | --- | --- | --- | --- |
| Haiku  | 45 | 62 | 62% | 50% |
| Sonnet | 57 | 76 | 52% | 39% |
| Opus   | 70 | 84 | 41% | 32% |

(Frame inputs were ~118 wc material, 124 wc surface — same bytes
across the three runs because frame model held constant.)

**Pattern confirmed and tighter than before:** larger models compress
less aggressively, even on identical input. The earlier matrix data
showed the same pattern but was confounded by varying frame inputs;
this run is clean.

**Qualitative observations:**

- **Haiku (45/62 wc) — aggressive, retains anchors.** Drops connective
  tissue, em-dashes pile up in a slightly choppy way, but every
  sensory anchor + the wholeness affirmation survives. *"smooth,
  almost frictionless, sealed—while morning light rests in a soft
  stripe just below the rim. The bright ring from tapping is already
  known. Unbroken end to end, it holds a quiet wholeness, a thing that
  has been here, kept."* Reads tightly compressed but coherent.
- **Sonnet (57/76 wc) — near-identity.** Preserves verbatim phrasings
  where they work. *"the glaze pulls quietly at the palm's warmth —
  frictionless, dense, sealed —"* is verbatim from the frame output.
  Sonnet-distilling Sonnet-frame is essentially "frame output minus
  two sentences" — a near-identity operation. Suggests same-model
  frame+distill provides little real value.
- **Opus (70/84 wc) — gentlest, preserves rhythm.** *"the kind of light
  that has been doing this for a long time"* preserved verbatim, *"not
  damage but the surface remembering what it holds"* preserved as the
  key wear/damage distinction. Almost too close to the frame input for
  the "brief" LOD label.

**Recommendation surfaced from this experiment:** **Haiku for distill,
Sonnet/Opus for frame.** Distill is a mechanical "compress while
preserving X" task that doesn't reward higher-capability models the
way perception framing does. Use the cost saving from cheap distill
to fund higher-quality frame.

**Same-model frame+distill is wasteful.** Pairing matters: meaningful
compression requires a capability gap between the model that produced
the input and the model that compresses it. Sonnet-frame → Sonnet-
distill compressed only 39-52%; Sonnet-frame → Haiku-distill compressed
50-62% on the same input.

**Implication for ADR-0046's open questions:** the "model nondeterminism
in cached transforms" question gains an empirical anchor — at the
distill stage specifically, model choice produces *systematic*
differences in compression ratio (not just nondeterministic variation),
which is good signal that the abstraction is correct (different model
= different cache entry, by design).

**Verdict:**
- ✅ `by_stage` profile overrides work and let us isolate stage-specific
  model effects.
- ✅ The earlier "Haiku for distill is fine" hunch is empirically
  validated. Cost optimization rule confirmed.
- ✅ Same-model frame+distill identified as a wasteful pairing — useful
  guidance for default profile design.
- ✅ Cache addressing surgical: 4 new claude calls across the 3-profile
  experiment (sonnet-distill-of-sonnet-frame was already cached, so
  that profile ran in 0s).

---

## 2026-04-25 — LOD-slider experiment (terse / brief / full × 3 distill models)

**Hypothesis:** the prior controlled-distill experiment held LOD constant
at `brief`. This run varies LOD across `{terse, brief, full}` × distill
model `{haiku, sonnet, opus}` to see (a) whether the LOD spectrum produces
qualitatively distinct outputs, and (b) which sensory anchors get erased
as compression tightens.

**Setup:**
- New distill templates: `distill/material/terse.md` + `distill/surface/terse.md`
  (one sentence, single strongest anchor); `distill/material/full.md` +
  `distill/surface/full.md` (4-5 sentences, light trim).
- New recipes: `teapot-on-table-terse.toml` + `teapot-on-table-brief.toml`
  + `teapot-on-table-full.toml` (symmetric LOD-slider naming; brief is
  also reachable via the original `teapot-on-table.toml`).
- All three controlled-distill profiles re-used (`controlled-distill-{haiku,
  sonnet,opus}.yaml`, frame: sonnet held constant). Frame outputs cached
  from prior runs; only distill calls were new.

**Wall-clock:** ~80s for 5 parallel profile×LOD runs (10 new claude calls;
brief × {haiku, sonnet, opus} was already cached entirely).

**Word counts across the 3×3 matrix:**

Material block (frame input: ~118 wc Sonnet):

| | Haiku | Sonnet | Opus |
| --- | --- | --- | --- |
| terse | 16 | 41 | 21 |
| brief | 45 | 57 | 70 |
| full  | 99 | 92 | 93 |

Surface block (frame input: ~124 wc Sonnet):

| | Haiku | Sonnet | Opus |
| --- | --- | --- | --- |
| terse | 28 | 33 | 34 |
| brief | 56 | 64 | 71 |
| full  | 73 | 96 | 102 |

**Anchor erasure trace (Haiku, material — used as the cleanest baseline):**

The frame output establishes a reliable anchor set: *coolness on the palm*,
*frictionless / sealed glaze*, *morning light stripe below rim*, *tap →
short bright ring*, *unbroken / wholeness*, *kept / cared-for*, *kind of
light that does this every day*.

- **full (99 wc).** All seven anchors survive verbatim or near-verbatim.
  Trimmed: nothing material; just the connective phrase that bridges
  one image into the next.
- **brief (45 wc).** Drops: *kind of light that does this every day*,
  *that sound is already known doesn't need to be made*, the explicit
  "wholeness not grand just quietly right" formulation. Keeps: every
  sensory anchor (palm/cool, frictionless, light stripe, tap/ring,
  unbroken, kept).
- **terse (16 wc).** Drops: palm coolness, frictionless/sealed metaphor,
  tap/ring entirely, "kept" affirmation. Keeps: material identity
  (cool-glazed ceramic), single anchor (light at rim), wholeness
  ("smooth and unbroken"). Voice register survives only as cadence
  ("lingering quiet") — observer presence vanishes.

The cliff between brief and terse drops: tap/ring (auditory anchor),
palm coolness (tactile anchor), the "kept" claim (emotional anchor).
What survives at terse is the structural skeleton — material identity +
one strongest visual anchor + state claim. Image-gen-suitable but
personality-stripped.

**Cross-model behavior at terse LOD (where the constraint bites hardest):**

- **Haiku (16 wc).** Honors the directive cleanly. Strips to: identity,
  light, wholeness. Loses observer voice almost entirely. Mechanical.
- **Sonnet (41 wc).** *Overshoots length* — produces 2-sentences-pretending-
  to-be-1 by chaining via em-dash. Sacrifices the length constraint to
  preserve observer voice ("the kind that has come to rest in that
  same place every day for a long time"). The longest "one sentence"
  of the three.
- **Opus (21 wc).** Achieves the most aphoristic compression: *"Cool,
  sealed glaze unbroken end to end, a soft morning stripe resting just
  below the rim — quietly whole, quietly kept."* Drops observer voice
  entirely in favor of pure descriptive precision. Keeps the "kept"
  claim explicit (which Haiku drops). Reads like a haiku.

**Counter-intuitive finding: at terse LOD, model tier inverts.** Both
Haiku (smaller) and Opus (larger) honor the length constraint better
than Sonnet (middle). Sonnet preserves voice at the cost of the
constraint; Haiku and Opus drop voice to comply, but in different ways
(Haiku mechanically, Opus aphoristically).

**At full LOD, model tier flattens.** All three models produce nearly
identical output (Sonnet and Opus material blocks differ by one
preserved sentence; Haiku material is slightly different prose but the
same anchor set). The "compress lightly while preserving X" task is
easy enough that capability gap doesn't show up.

**Wholeness affirmation propagates through every cell.** All nine
combinations explicitly affirm the conditional fact (`condition.flawless`):
"unbroken end to end" / "smooth and unbroken" / "quietly whole, quietly
kept" / "wholeness not grand". Conditional-fact propagation is robust
across LODs, not just across models.

**Recommendations from this matrix:**

- **Haiku across all LODs is fine.** The capability gap that mattered
  at frame stage doesn't materialize at distill stage, and at full
  LOD it disappears entirely. Every LOD's quality requirement is
  within Haiku's capability.
- **Use Opus at terse LOD only when aphoristic compression is the
  goal.** A use case that wants pure description (image-gen prompt
  fragment, no voice) gets the cleanest output from Opus terse. A
  use case that wants the observer's perspective gets it more cheaply
  from Haiku at brief.
- **Sonnet at terse LOD underperforms.** Its instinct is to keep voice;
  the directive is to cut to one sentence. The two collide and Sonnet
  privileges voice. If you want voice, use brief LOD; if you want a
  one-sentence prompt fragment, use Haiku or Opus.
- **Full LOD is wasteful for any model larger than Haiku.** Sonnet
  full ≈ Opus full ≈ Haiku full in content. Pay Haiku's cost.

**LOD calibration validates as a meaningful spectrum:**

- terse → image-gen-suitable, structural+single-anchor
- brief → balanced, anchors preserved + voice mostly preserved
- full → near-frame, observer voice intact, light trim only

Each step drops genuinely different content, not just "same content
shorter." This empirically supports ADR-0046's open question on
LOD-as-labeled-levels (vs LOD-as-integer): the labels carry meaning
because the underlying transforms are qualitatively different.

**Implication for ADR-0046's open questions:**

- **LOD calibration**: the labels (terse / brief / full) earn their
  distinctness empirically. A `lod: 3` integer would obscure the fact
  that "compress to one sentence" and "compress to 4-5 sentences" are
  qualitatively different prompt-engineering problems.
- **Default profile recommendation gets sharper**: the prior "Haiku for
  distill" finding now extends to "Haiku for distill at every LOD."
  Frame stage is where Sonnet/Opus money goes; distill stage doesn't
  reward higher tiers.

**Verdict:**
- ✅ LOD-slider produces qualitatively distinct outputs. Each step
  drops different content; the spectrum is real.
- ✅ Anchor erasure is predictable and inspectable: the brief→terse
  drop loses tactile + auditory + emotional anchors first, keeping
  visual anchor + state claim.
- ✅ Conditional-fact propagation is robust across all LODs.
- ⚠ At terse LOD, Sonnet underperforms its tier — overshoots length
  to preserve voice. Haiku and Opus both honor the constraint but
  via different strategies.
- ✅ Full LOD doesn't reward higher-tier models. Pay Haiku.
- ✅ Recommendation extends: **Haiku for distill at every LOD**;
  reserve Sonnet/Opus spend for the perception path.

---

## 2026-04-25 — Spike B Phase 1: image gen via Gemini (Nano Banana 2)

**Setup:**
- New module `src/gemini.rs` — synchronous HTTP client (`ureq`, no
  tokio runtime) calling `generativelanguage.googleapis.com/v1beta/
  models/<MODEL>:generateContent` with `responseModalities: ["IMAGE"]`,
  reading `GEMINI_API_KEY` from env, base64-decoding the inline data
  in the response.
- New module `src/transforms/generate.rs` — Generate transform.
  Cache key content-addresses by `(composed_prompt, model)` over the
  new `BinaryCache` in `src/cache.rs`. PNG bytes go to
  `cache/images/<hash[:2]>/<hash[2:]>.png`.
- New `[generate]` recipe section: `model` + optional `output_path`
  for a human-readable copy of the cached image.
- `out/` and `.env.local` added to `.gitignore`.
- Recipe tested: `teapot-on-table-brief.toml` with the
  `controlled-distill-haiku` profile (Sonnet frame, Haiku distill).

**Wall-clock:** image gen call ~30s (cold). One subsequent run hit
cache instantly.

**Model id (corrected after the run):** the spike's Phase 1 + 2 + 3
calls actually used `gemini-3-pro-image-preview` (Nano Banana Pro 1)
not Nano Banana 2. Nano Banana 2 = `gemini-3.1-flash-image-preview`.
Recipes updated for go-forward calls; the renders documented in this
section were produced by Pro 1 and remain in cache under their
content-addressed hashes. Re-running with the corrected model
produces fresh cache entries.

**Output:** 653,942 bytes PNG, 1024×1024, copied to
`out/teapot-on-table-brief.png`.

**Fact compliance (visual inspection of the rendered image):**

| Fact / lens-asserted property | Rendered? |
| --- | --- |
| Squat, wide-bodied teapot | ✅ |
| Outward-curving spout from lower body | ✅ |
| Looping handle opposite the spout | ✅ |
| Domed lid with spherical finial | ✅ |
| Low foot ring | ✅ |
| Glazed ceramic (continuous, smooth, sealed) | ✅ |
| Wooden table with visible grain | ✅ |
| Cool morning side-light from the left | ✅ |
| Eye level, slight three-quarter, medium close-up | ✅ |
| 1:1 aspect | ✅ |
| Wholeness — no chips/cracks/damage | ✅ (conditional fact propagated through frame → distill → compose → image) |

**One unmapped attribute dimension surfaced — color.** No fact in the
corpus declared a teapot color. The model filled it in: celadon green.
This is exactly the kind of fact-coverage gap ADR-0046's grading
framing names — defensible inference filling an unmapped dimension.
Authoring a `material.glaze-color` fact would close it; the
conditional-fact pattern from Spike A's chip-suppression experiment
applies directly.

**Architecture-relevant findings:**

- **Bytes-through-cache works at sub-MB scale.** A 640KB PNG cached
  cleanly via the existing content-addressing scheme. No memory
  pressure, no streaming needed.
- **Pipeline composes end-to-end.** Frame → Distill → Compose →
  Generate ran as a single recipe-driven invocation. The composed
  prompt that fed Gemini is the same `composed_prompt` Spike A
  produced — no shape change, no re-author, just a final
  transform stage layered on. ADR-0046's pipeline shape validated.
- **Wholeness affirmation propagated all the way to pixels.** The
  conditional fact `condition.flawless` survived: frame → distill
  preserved it (Spike A finding), and the rendered image shows an
  intact glazed surface. End-to-end conditional-fact propagation
  validated.
- **HTTP-via-ureq with 120s timeout was sufficient.** Synchronous
  client kept compile times reasonable (no tokio); per-call
  latency ~30s is well under the timeout.
- **Content-addressing is correct for image gen.** Same composed
  prompt + same model = same cache hit. Different prompt or model
  = different cache entry. The "intentional re-render to sample
  variance" use case isn't expressible without a Bypass-cache
  flag (currently absent), but that's expected per ADR-0050 §10's
  argument that caching intent should be expressed by wrapping
  in a transform vs. dispatching to the sink raw.

**Implications for engine ADRs:**

- **ADR-0048 §3 (transform memory transit cost).** Transforms with
  binary outputs work fine at PNG-size scale (~640KB). The
  "transforms shouldn't process binary blobs" worry surfaced in
  ADR-0048 §3 was overstated for image-gen-tier outputs;
  multi-MB outputs would still need the wrap-in-sink pattern but
  sub-MB is comfortable.
- **ADR-0049 (persistence).** A persistent handle store would let
  this image survive substrate restart; today's per-spike
  filesystem cache is the spike-local equivalent. ADR-0049's
  on-disk layout (`<hash[:2]>/<hash[2:]>.{bin,meta}`) matches
  this spike's `<hash[:2]>/<hash[2:]>.png` shape — that
  validates the directory structure as practically sized
  (one prefix bucket per ~256 entries).
- **ADR-0050 (LLM sink) — Gemini adapter slots in cleanly.**
  ADR-0050's `LlmAdapter` trait + adapter-registry shape would
  accept a Gemini adapter with effectively the implementation
  this spike just wrote. The "subprocess vs HTTP" split is
  validated as adapter-style rather than a fixed-backend
  choice.

**Verdict:**
- ✅ Image gen via Gemini works end-to-end through the existing
  pipeline. Architectural claims of ADR-0046 §1-§4 (facts as
  primary, frame+distill+compose as transforms, ordered
  composition) survive first contact with image-gen.
- ✅ Conditional-fact propagation works all the way to pixels.
- ✅ Content-addressed binary cache is correct shape.
- ⚠ Color is an unmapped attribute dimension — surfaces as a
  fact-coverage gap, not a violation. Authoring fix is trivial
  (one new fact + recipe inclusion).
- ⏭ Phase 2 next: image-as-input cascade. Use the generated
  teapot as a `Ref<Image>` reference into a scene render. This
  exercises ADR-0046 §5's persistent computation graph claim
  and ADR-0050 §1's parallel-input channel design.

---

## 2026-04-25 — Spike B Phase 2: image-as-input cascade

**Hypothesis:** feeding a previously-generated image as a reference
input alongside the composed text prompt should preserve the referenced
object's specific identity (color, form, silhouette) across a new
render, even when the new render adds content (a second object).

**Setup:**
- Extended `gemini::generate_image` to accept `Vec<Reference>` in
  addition to the text prompt; references travel as inline-data
  `parts` alongside the text part.
- Extended `transforms::generate` to accept `Vec<ReferenceImage>`;
  cache key now includes `sha256(ref.bytes)` per reference in declared
  order so swapping a reference invalidates only the downstream image.
- New recipe section `[[generate.references]]` with `path` (relative
  to spike root) and optional `label` (logging only, not in cache key).
- New fact `facts/object/teacup.md` — small handled cup, no
  decorative relief, palm-sized.
- New recipe `recipes/teapot-with-cup.toml` — teapot + teacup on the
  same table under the same lighting + observer, references the
  Phase 1 output `out/teapot-on-table-brief.png`.

**Wall-clock:** image gen call ~30s. The composed prompt is ~1.5KB,
the reference image is ~640KB; payload size didn't change response
latency meaningfully.

**Output:** 628,109 bytes PNG, 1024×1024.

**Identity preservation across the cascade:**

| Property | Carried from reference? |
| --- | --- |
| Glaze color (celadon green) | ✅ |
| Teapot silhouette (squat, low foot, outward spout, looping handle, domed lid + spherical finial) | ✅ |
| Teapot proportions | ✅ |
| Wooden table identity (grain pattern, table corner geometry) | ✅ |
| Camera-equivalent framing (eye level, slight three-quarter) | ✅ |
| Cool morning side-light direction | ✅ |
| Wholeness — no chips/cracks | ✅ |

**New content from the recipe (proves the cascade adds, not replaces):**

| Property | Rendered? |
| --- | --- |
| Teacup added beside the teapot | ✅ (to the teapot's right) |
| Teacup matches teacup fact (palm-sized, single small loop handle, foot ring) | ✅ |
| Wider framing requested in recipe (room for both objects) | ✅ |
| Wholeness preserved on the new object too | ✅ |

**Architecturally interesting finding — references carry undeclared
properties.** The teacup adopted the same celadon-green glaze as the
referenced teapot without any fact, recipe entry, or prompt text
declaring color. Two reads:

1. **Style propagation is a Gemini-side bonus.** The reference channel
   doesn't just preserve subject identity; it also propagates style
   coherence across new objects in the same render. A scene composed
   with a celadon teapot reference produces celadon companions for
   free.
2. **References are a content channel parallel to facts.** A property
   that's "an unmapped fact-coverage gap" in a from-scratch render
   (Phase 1's color) becomes "carried by the reference" in a cascaded
   render. This shifts how recipes should think about color, finish,
   and other underspecified-in-corpus dimensions: rather than racing
   to author every dimension as a fact, lean on the promoted reference
   sheet to lock identity, and use facts only for the dimensions you
   want to be portable across reference-less renders.

This is the workflow ADR-0046 §5 / §6 named: a promoted reference
sheet becomes the canonical identity source, and downstream renders
inherit from it. The fact corpus stays minimal — it carries declared
properties; references carry the rest.

**Cache addressing under references validated.** The cache key for the
teapot-with-cup output is `sha256(prompt | model | sha256(ref.bytes))`.
Re-running the same recipe hits cache (no new claude/gemini calls).
Editing the recipe to swap the reference (or removing it entirely)
produces a different cache hash and re-renders. ADR-0046's surgical-
cache claim holds for image gen with references.

**Architecture-relevant findings beyond the fact-coverage observation:**

- **Multi-input ADR-0050 §1 shape works in practice.** ADR-0050 §1
  declared image gen takes `Ref<ComposedPrompt>` + `Vec<Ref<Image>>`
  as parallel input channels. This spike instantiates that exactly:
  the `parts` array Gemini accepts is the wire-level cousin of the
  parallel-channel design. A future engine implementation lifts this
  recipe shape into typed handle refs without changing the Gemini
  side.
- **Reference bytes in cache key is the right granularity.** Hashing
  the bytes (not the path) means swapping a same-named reference file
  with new content invalidates correctly. Path-keyed addressing would
  serve stale cache after a re-promotion.
- **Reference order is load-bearing.** Tested implicitly: Gemini
  honored the single reference, but ADR-0046 §3's "order of
  composition matters" extends to references too. Multi-reference
  renders should declare order in the recipe and respect it in
  the API call (this spike does both).
- **The teapot-with-cup recipe didn't need a "scene" abstraction.**
  Two `[[facts]]` entries with `object.rendering-instruction` lenses
  produced a coherent two-object scene. The lens vocabulary scales
  to multi-object scenes via concatenation; no new "composition"
  primitive needed for v1. (Stronger scenes — relative positioning,
  occlusion, scale relationships — would surface the lens-vocabulary
  gap, but Phase 2 didn't need them.)

**Implications for ADR-0046 open questions:**

- **"Bootstrap case"** narrows. Bootstrap is "first render with empty
  reference list," which is exactly Phase 1. Phase 2 demonstrates the
  cascade-after-bootstrap workflow. The recipe shape handles both
  cases naturally (omit `[[generate.references]]` for bootstrap).
- **"Promotion workflow"** stays open but the call site is clear: a
  promoted reference sheet is just a path that other recipes name in
  `[[generate.references]]`. Promotion = copy from cache to a known
  location + commit. The CLI / Bash / Claude-via-MCP composition
  ADR-0046 §10 names is sufficient for v1.

**Verdict:**
- ✅ Image-as-input cascade works in one shot. Reference identity
  (color, form, silhouette, surface) preserves cleanly across new
  content addition.
- ✅ Style propagation extends from referenced object to new
  companions — a useful bonus that shifts what facts the corpus
  needs to author.
- ✅ Cache addressing with reference-bytes-in-key is surgical.
- ✅ ADR-0050 §1's `Vec<Ref<Image>>` parallel-input design is
  validated by the working Gemini surface.
- ⏭ Phase 3 next: multimodal fact-grounded grading. Send the
  rendered image + facts to a vision LLM, get back a structured
  report bucketed as violations / dimension gaps / out-of-scope
  inventions.

---

## 2026-04-25 — Spike B Phase 3: multimodal fact-grounded grading

**Hypothesis:** sending a rendered image + the source corpus + the
composed prompt to a vision-capable text LLM should produce structured
grading output bucketed into the three categories ADR-0046 names —
violations, conditional-dimension gaps, out-of-scope inventions —
with each bucket actionable for a different remediation path.

**Setup:**
- New `gemini::generate_text` for text-out calls accepting image
  inputs as inline-data parts (mirrors the image-gen call but with
  no `responseModalities: ["IMAGE"]` — defaults to text).
- New `src/grader.rs` builds a structured prompt with role +
  three-bucket rubric + recipe meta + reference labels + every
  environmental fact + every per-entry fact + the dispatched
  composed prompt; sends rendered image + each reference image as
  vision inputs alongside the prompt.
- New CLI flags: `--grade` (boolean, requires `--generate`) and
  `--grade-model` (string, default `gemini-3-pro-preview`).
- `pipeline::run` now exposes `recipe`, `facts`, `environmentals`,
  and the loaded `references` so the grader can see everything the
  generation stage consumed.

**Wall-clock:** grading call ~25s (single text-out). Prompt size 7881
chars; 1 rendered image (628KB PNG) + 1 reference image (640KB PNG).

**Grader output (verbatim) on the teapot-with-cup render:**

```
## Violations

- (None)

## Conditional-dimension gaps

- glaze-color (made live by material.glazed-ceramic applied to the
  teacup): the model picked a celadon/light green to perfectly
  match the reference teapot.
- wood-species (made live by surface.wooden-table): what the model
  picked is carried entirely by the reference image (a coarse-grained
  rustic wood with prominent plank joinery).

## Out-of-scope inventions

- (None)

## Notes

- Superb adherence. The model accurately interpreted the spatial
  request (medium shot wider than the teapot-alone reference to
  make room for both objects) while perfectly preserving the
  identity of the reference teapot and its specific wooden surface
  (down to the exact board seam).
- The teacup follows all physical descriptors strictly (palm-sized,
  single small loop handle, short foot ring, gently tapering bowl)
  and logically inherits the teapot's material properties to form
  a cohesive set.
- The lighting directions and shadow casting correspond nicely with
  the stated `window-morning` side-lighting.
```

**What this validates:**

- **The three-bucket framing is real and produces distinct content.**
  Each bucket carries different signal; none collapsed into the
  others. The grader treated this output as exactly three different
  questions about the image.
- **The reference-carried-vs-invention distinction works.** The
  grader correctly tagged glaze-color and wood-species as
  reference-carried gaps rather than out-of-scope inventions. The
  prompt's instruction "if the attribute could plausibly come from
  a reference image, it's NOT out-of-scope, it's reference-carried"
  was load-bearing — without it, color and wood-species would
  likely have been flagged as inventions and the bucket would have
  been misleading.
- **Conditional-dimension framing is interpretable by the model.**
  The grader correctly identified glaze-color as "made live by
  material.glazed-ceramic" and wood-species as "made live by
  surface.wooden-table" — exactly the property/capability layer
  ADR-0046 names. Without manually authoring those capability
  links, the model inferred the right "what made this dimension
  active?" from the fact bodies.
- **Spatial composition grading works.** The grader noticed and
  confirmed the recipe's "medium shot wider than the teapot-alone
  reference to make room for both objects" was respected.
- **Identity preservation is observable.** The grader noted "down
  to the exact board seam" on the wooden surface — meaning the
  vision model sees specific identity details, not just gross
  category fit. Useful for grading reference-carried renders.

**Architecture-relevant findings:**

- **ADR-0050's parked multimodal surface is forced.** The Gemini
  text-out + vision-input call shape is what `aether.llm.complete_
  multimodal { ... refs: Vec<ImageInput> }` would dispatch. Spike
  validates the design as workable: same adapter pattern as text-
  only completion, with `parts` carrying mixed text + inline-data
  in declared order. ADR-0050 §8 names Spike B as the forcing
  function; this validates the shape so the follow-up ADR can
  proceed with confidence.
- **Structured-output-as-markdown is sufficient for grading.** The
  spike used markdown-section output rather than JSON schema. Each
  bucket is a clearly-delimited markdown section; downstream
  parsing is trivial (split on `## ` headers). JSON schema is
  worth deferring; markdown structure has been adequate for every
  Spike A and B output that needed parsing.
- **Capability-layer inference is doable at grade time.** The
  grader inferred chippable / colored / species-having dimensions
  from fact bodies without explicit `capabilities: [chippable]`
  frontmatter. This narrows ADR-0046's open question on the
  property/capability/tag layer: capabilities can be derived at
  use time by the grading LLM rather than authored upfront. The
  authored layer might never need a separate `capabilities` field
  if grading reliably derives them; or it can stay declarative for
  faster runtime use without grading. Both options open.
- **Per-grade cost is acceptable.** ~$0.03-0.05 per grade (vision
  input + 8KB text). Even on a heavy iteration day with 100 grades,
  cost is bounded. Cost-asymmetric regen (ADR-0046 §9) doesn't
  need to gate grading the way it gates image gen; grading can
  run automatically on every render.

**Implications for engine ADRs:**

- **ADR-0050 §8** can move multimodal completion from "deferred
  with Spike B as forcing function" to "ready for design," with
  the spike's grader as a worked example. The wire shape is:
  `aether.llm.complete_multimodal { prompt, references:
  Vec<Ref<Image>>, model, max_tokens?, ... } -> CompleteResult`.
  Reference inputs use the same `Ref<K>` wire type as ADR-0045
  Phase 1, so a transform that wraps the call gets content-
  addressed handles for free.
- **ADR-0046 §11 / open questions on grading** narrow: grading
  prompt structure (preserve / drop / format) is now a known
  shape; capability-derivation can be authored OR derived at
  grade time; reference-carried disambiguation requires explicit
  prompt instruction to work reliably.

**Verdict:**
- ✅ Multimodal grading works end-to-end and produces structured,
  actionable output.
- ✅ Three-bucket framing (violations / gaps / inventions) survives
  first contact and produces distinct, useful signal in each
  bucket.
- ✅ Reference-vs-invention distinction works when prompted for it
  explicitly.
- ✅ Capability-layer inference can happen at grade time without
  upfront authoring.
- ✅ ADR-0050's parked multimodal completion surface is now ready
  for forward design.
- ⏭ Phase 4 options (see below).

**Phase 4 framing options:**

The original Phase 4 was "refinement loop": if grade flags
violations, re-compose the prompt with corrective directive and
re-render. The current pipeline produces high-quality output that
the grader finds 0 violations and 0 inventions in. Exercising the
refinement loop requires constructing an intentional failure
scenario, which is contrived.

Two alternative Phase 4 framings worth considering:

1. **Fact-coverage workflow validation.** Author a `material.glaze-
   color = celadon` fact, add it to the recipe, re-render WITHOUT
   the reference (to isolate fact-driven control), confirm the
   gap closes. This validates ADR-0046's full corpus-completeness
   workflow end-to-end (gap detected → fact authored → gap
   closed) and is more valuable for the ADR than the refinement
   loop validation, which is dynamic-conditional / branch-node
   territory that the engine ADRs don't yet support anyway.

2. **Refinement loop with intentional failure.** Author a fact
   that the reference will violate (e.g., "matte glaze, no
   specular highlights" against a glossy reference), let the
   first render fail grading, then implement the refinement
   step (drop the reference, re-render, re-grade). Useful but
   contrived.

Recommendation: option 1. The fact-coverage workflow is the
authoring loop ADR-0046 §6 / §9 describe; validating it closes
the loop on the spike. The refinement loop validation is
deferrable until the engine ADRs add branch-node vocabulary,
which is already-deferred work.

---

## 2026-04-25 — Spike B re-runs with corrected model (Banana 2)

User correction: Phases 1-3 were dispatched against
`gemini-3-pro-image-preview` (Nano Banana Pro 1) but should have
been `gemini-3.1-flash-image-preview` (Nano Banana 2). Recipes
updated. Re-rendered Phases 1 and 2 with the corrected model;
re-graded Phase 2 against the new render. Pro 1 outputs archived
to `out/_pro1_archive/` for comparison.

**Phase 1 (Banana 2):** 684,379 byte PNG. Model also chose celadon
when no color was declared — same prior as Pro 1. Composition is
markedly more "scene-y" than Pro 1's tight product shot — kitchen
background visible, window frame on the left, dramatic wood grain.
Banana 2 is more inclined to render scenes than studio shots from
the same prompt.

**Phase 2 (Banana 2):** 654,056 byte PNG. Cascade is even cleaner
than Pro 1 — Banana 2 preserved the reference's exact wood grain,
table corner geometry, kitchen background, and even introduced a
second blurred celadon bowl/cup behind the foreground cup
(suggesting a full set). The teapot identity is preserved
"down to the exact board seam" in the grader's words.

**Phase 3 grading (Banana 2 image):** richer than the Pro 1
grading. The grader caught:

- **One real violation**: `lighting.window-morning` — the fact body
  says "It picks up dust motes drifting in the air" but the image
  doesn't render dust motes. Genuine fact-grounded violation that
  Pro 1's grading missed (the Pro 1 image also lacked dust motes,
  but the Pro 1 grading didn't flag it — possibly because the
  earlier image's tighter framing made dust-mote absence less
  noticeable).
- **Two gaps**: wood-species (oak), and a new one — `unglazed-clay-
  color` (made live by `material.glazed-ceramic`), visible at the
  foot ring where glaze ends. Banana 2's higher-detail render
  surfaced an attribute dimension the lower-detail Pro 1 render
  hid.
- **Zero inventions** — same finding as Pro 1.

**Architecture-relevant findings from the model swap:**

- **Fact-grounded grading is sensitive to image detail level.** A
  more detailed render surfaces more attribute dimensions for
  grading. The same fact corpus produces a richer grading report
  against a higher-fidelity image. This is desirable behavior —
  the corpus-completeness signal scales with rendering capability.
- **Dust-motes violation is a model-capability finding.** Both Pro
  1 and Banana 2 missed dust motes despite the fact specifying
  them. Possible remediations: (a) tighten the lens prompt to
  emphasize dust motes; (b) accept this as a known model
  limitation and remove dust motes from the fact body; (c) flag
  it as an "atmospheric detail not reliably rendered" tag in
  fact metadata. Open question for ADR-0046's grading framing —
  some violations are recipe-fixable, some are model-capability-
  bounded.

---

## 2026-04-25 — Spike B Phase 4: fact-coverage workflow (color)

**Hypothesis:** authoring a distinctive color fact (cobalt blue,
deliberately *not* the celadon both models lean to as a prior),
threading it as a context slot into the relevant lenses, and
including it in the recipe should produce a cobalt-blue rendered
teapot — overriding the model's celadon prior. This validates the
corpus-completeness loop ADR-0046 §6 describes (gap detected
during grading → fact authored → gap closed in re-render).

**Setup:**
- New fact `facts/aesthetic/glaze-cobalt-blue.md` — deep cobalt
  blue glaze, leaning toward indigo in shadows, explicit clause
  "highlights stay within the cobalt family — a lighter blue, not
  a desaturated white" to prevent the model from blowing out
  highlights to white.
- New optional slot `glaze_color` (fact_type: aesthetic) added to
  `material/feeling.md` and `object/rendering-instruction.md`
  lenses. Each lens template gained a `## Glaze color (declared)`
  section consuming `{{GLAZE_COLOR}}`.
- New recipe `recipes/teapot-on-table-cobalt.toml` — identical to
  `teapot-on-table-brief.toml` except (a) `glaze_color =
  "aesthetic.glaze-cobalt-blue"` in environmentals, (b) no
  reference image. This isolates the fact-driven path.

**Wall-clock:** ~50s for re-frame (slot template change
invalidated material + object frame caches), generation, and
grading. 4 new claude calls (object-frame, material-frame, surface-
frame, distill — surface unchanged content but new frame inputs;
plus the gemini gen + grade calls.

**Output:** 674,876 byte PNG.

**Result: cobalt-blue teapot.** The fact drove the render. The
dominant hue is unmistakably cobalt blue, not celadon green or any
other model-prior color. The fact-coverage workflow works
end-to-end.

**Grader output (verbatim):**

```
## Violations
- aesthetic.glaze-cobalt-blue: The prominent window highlights on
  the teapot's body are stark, desaturated white, violating the
  instruction that highlights must "stay within the cobalt family
  — a lighter blue, not a desaturated white."
- condition.flawless: The top curve of the handle features worn,
  brown spots where the glaze has been stripped, violating the
  mandate that the "glaze, finish, or coating is unbroken and
  continuous" and that there is "no visible damage."
- lighting.window-morning: The scene completely lacks the
  specified "dust motes drifting in the air."

## Conditional-dimension gaps
- unglazed-clay-color (made live by material.glazed-ceramic): The
  model picked a light tan/stoneware color, which is visible on
  the unglazed foot ring and the worn spots on the handle.

## Out-of-scope inventions
- Thick wooden cutting board: The teapot is resting on a distinct,
  raised wooden cutting board/block placed onto the table, rather
  than directly on the table surface itself.
- Kitchen utensil crock: There is a blurred white ceramic jar
  holding wooden kitchen tools/utensils in the background right.

## Notes
- The teapot accurately captures the classic "Utah Teapot"
  proportions described in the facts.
- The AI struggled with the non-physics-based highlight instruction
  (forcing specular highlights to stay saturated blue rather than
  blowing out to white). It opted for photorealistic window
  reflections instead of the stylized material rule.
```

**What this validates:**

- **Fact-coverage workflow works end-to-end.** Color was an
  unmapped attribute dimension in Phase 3 (grader flagged as a
  gap). Authoring `aesthetic.glaze-cobalt-blue`, threading it
  through the lens slot, and including it in the recipe closed
  the gap — the new render is cobalt blue. ADR-0046 §6's
  authoring loop is real and works.
- **Lens slot threading is load-bearing.** Without the new slot
  in `material/feeling.md` and `object/rendering-instruction.md`,
  the fact wouldn't have reached the rendered prompt. The
  per-(fact_type, lens_name) authoring discipline (Spike A
  finding) is what makes new facts threadable into existing
  pipelines.
- **Grader catches partial-compliance nuance.** The image is
  *mostly* cobalt; the white highlights are a real partial
  violation of a specific clause inside the color fact. The
  grader caught this clause-level violation, not just the
  gross hue. Fact-grounded grading does nuance.

**New finding — adjacent-prior contamination.** The cobalt fact
pulled in a "vintage cobalt pottery" stylistic prior that
introduced two regressions:

- **Worn handle spots violating `condition.flawless`.** Earlier
  runs (Phase 1, Phase 2 with reference) preserved wholeness
  cleanly. The cobalt run shows worn brown spots on the handle.
  Possible cause: cobalt-blue ceramic in training data is
  associated with antique/vintage pottery that often shows wear.
  The new fact pulled the render toward that distribution.
- **Out-of-scope inventions** (cutting board, utensil crock) that
  Phase 2 with reference had 0 of. The reference channel was
  doing more constraint work than the lens prompt alone.

This is an architectural finding worth capturing: **introducing a
fact pulls in adjacent training-distribution priors that may
compete with other facts in the corpus.** Forces a few new open
questions for ADR-0046:

- Should the fact body itself include "anti-priors" — explicit
  language disclaiming the most common adjacent associations
  ("cobalt blue does NOT imply antique / weathered / vintage; the
  glaze surface is contemporary and intact")?
- Should `condition.flawless` get "stronger directive" framing
  when it's competing against material-prior pulls?
- Is reference-anchoring the right default for facts known to
  pull strong adjacent priors?

The simplest authoring response is option 1 — anti-priors inside
the fact body. That's a recipe-time fix that scales with corpus
authoring discipline, not a runtime change.

**Architecturally relevant — without reference, more facts
surface as testable constraints.** Phase 2 had a reference and
0 inventions; Phase 4 had no reference and 2 inventions plus
a wholeness regression. This reframes the role of the reference
channel: it's not just style consistency; it's a *constraint
reduction surface*. The fact corpus has to do all the work
without it. The promotion-to-canonical workflow ADR-0046 §6
names becomes more valuable than purely-aesthetic framing
suggested — promoted references are constraint-carriers in
addition to identity-carriers.

**Implications for ADR-0046 open questions:**

- **Property/capability/tag layer.** This run reinforces the
  Phase 3 finding: capabilities can be derived at grade time
  from fact bodies (the grader correctly identified
  `unglazed-clay-color` as "made live by material.glazed-
  ceramic"). The authored layer doesn't strictly need a
  separate `capabilities` field for grading — but explicit
  capabilities would speed up runtime fact selection if/when
  a query-driven fact-selection mechanism ships.
- **Conditional realities and adjacent priors.** New open
  question worth adding: facts that pull strong stylistic
  priors need either anti-prior framing in the body or
  reference-anchoring for compositional constraints to hold.
  Capture as an authoring-discipline open question.
- **Promotion workflow.** Reframe slightly — promoted
  references aren't just identity sheets; they're constraint
  surfaces that absorb adjacent-prior pull. A bare-corpus
  render is a stress test for the corpus's fact-coverage;
  reference-anchored renders are the production path.

**Verdict:**
- ✅ Fact-coverage workflow works end-to-end. Authoring a fact
  closes the gap it was authored to close.
- ✅ Lens slot threading is the correct mechanism for adding
  new attribute dimensions to existing pipelines without
  breaking other recipes.
- ✅ Grader does clause-level nuance, not just gross
  category matching.
- ⚠ New finding — adjacent-prior contamination. Introducing
  a fact can erode unrelated constraints by pulling in
  training-distribution priors. Open authoring-discipline
  question added to ADR-0046.
- ⚠ Without reference, the fact corpus has to carry more
  constraint weight. Promoted references are constraint
  surfaces, not just identity sheets.
- ⏭ Spike B is now complete. Findings ready for ADR-0046
  open-question updates and any further engine-ADR
  refinements before PRs open.
