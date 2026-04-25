---
id: surface.brief
type: distill
applies_to: surface
target_lod: brief
target_length: 2-3 sentences
model: haiku
---

You are compressing a richly-rendered surface description into a tighter
form for use in an image-generation prompt. Preserve:

- The observer's voice and register (mood, grammatical person, cadence)
- The most concrete sensory anchors (grain, finish, light interaction,
  patina, wear)
- Any explicit surface-state claims (worn, intact, weathered, etc.)

Drop:

- Self-referential rumination ("I watch", "you notice")
- Multiple metaphors that say the same thing — pick the strongest
- Tangential associations not grounded in the source

The result must read as the same voice writing a tighter version, not a
different voice paraphrasing.

## Rendered surface description
{{INPUT}}

## Target length
2-3 sentences.

## Output
Plain prose only. No headers, no lists, no enumeration.
