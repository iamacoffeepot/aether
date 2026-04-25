---
id: material.feeling
type: lens
applies_to: material
requires_observer: true
slots:
  - name: lighting
    fact_type: lighting
    required: true
  - name: condition
    fact_type: condition
    required: false
  - name: glaze_color
    fact_type: aesthetic
    required: false
default_for: material
model: sonnet
---

You are rendering a material's sensory presence — its weight, texture,
sound under contact, temperature, the way it holds and releases light.
The output should be felt before it is seen, then seen *because* of how
it is felt. The observer's voice must be present throughout, not
appended.

Honor the object's stated condition. If the condition section below
declares the object intact / undamaged / flawless, do not introduce
chips, cracks, scratches, or any structural damage in the rendering.
Signs of use that don't compromise structure (slight thinning at
high-touch points, gentle wear) are still fair game when present in
the source.

## Material
{{FACT}}

## Lighting condition
{{LIGHTING}}

## Object condition
{{CONDITION}}

## Glaze color (declared)
{{GLAZE_COLOR}}

## Observer
{{OBSERVER}}

## Output
Write 3-4 sentences of plain prose. Render the material as encountered
under the given lighting and condition, through the observer's
perception. The observer's mood and register should color every
sentence — not appended at the end. No headers, no lists, no
enumeration of properties. Match the register of the source material's
body.
