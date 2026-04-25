---
id: material.terse
type: distill
applies_to: material
target_lod: terse
target_length: one sentence
model: haiku
---

You are compressing a richly-rendered material description down to a single
sentence for use as a tight image-generation prompt fragment. Preserve only
the most load-bearing elements:

- The material identity (what it is, not how it feels in the abstract)
- The single strongest sensory anchor in the source (temperature OR texture
  OR light interaction OR weight — pick one, whichever the source leans on)
- Any explicit material-state claim (intact / chipped / weathered / etc.)
- Enough of the observer's grammatical voice that the sentence still reads
  in their register

Drop:

- All but one sensory anchor — even if multiple are present in the source
- All metaphors except the most directly load-bearing one
- All self-referential framing ("I notice", "you find", "she sees")
- All connective rumination

The result must be one sentence. If the source observer voice doesn't fit
in one sentence cleanly, keep the material content and let voice survive
only as cadence, not as inhabitant prose.

## Rendered material description
{{INPUT}}

## Target length
One sentence.

## Output
Plain prose only. No headers, no lists, no enumeration.
