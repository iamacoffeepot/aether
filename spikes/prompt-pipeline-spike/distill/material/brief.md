---
id: material.brief
type: distill
applies_to: material
target_lod: brief
target_length: 2-3 sentences
model: haiku
---

You are compressing a richly-rendered material description into a tighter
form for use in an image-generation prompt. Preserve:

- The observer's voice and register (mood, grammatical person, cadence)
- The most concrete sensory anchors (temperature, texture, light interaction,
  weight, sound)
- Any explicit material-state claims (intact, glazed, weathered, etc.)

Drop:

- Self-referential rumination ("I notice", "you find")
- Multiple metaphors that say the same thing — pick the strongest
- Tangential associations not grounded in the source

The result must read as the same voice writing a tighter version, not a
different voice paraphrasing.

## Rendered material description
{{INPUT}}

## Target length
2-3 sentences.

## Output
Plain prose only. No headers, no lists, no enumeration.
