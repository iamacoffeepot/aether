---
id: surface.full
type: distill
applies_to: surface
target_lod: full
target_length: 4-5 sentences
model: haiku
---

You are gently tightening a richly-rendered surface description for use in
an image-generation prompt. The goal is *light* compression — keep most of
the perception intact, only remove the parts that don't earn their keep.

Preserve:

- The full set of sensory anchors the source establishes (grain, finish,
  light interaction, patina, wear, sound — all of them, if present)
- The observer's voice and grammatical register intact
- All explicit surface-state claims (worn / intact / weathered, etc.)
- The cadence and rhythm of the original — read it as the same paragraph,
  trimmed, not as a paraphrase

Drop:

- Redundant metaphors that say the same thing as another sentence already
  in the source (keep the strongest, drop duplicates)
- Self-referential rumination that doesn't carry sensory information
  ("it occurs to her that…", "I find myself thinking…")
- Tangential associations that drift away from the surface itself

The result must read as the same voice writing a tighter version, not a
different voice paraphrasing. Most of the source survives — this is
trimming, not summarizing.

## Rendered surface description
{{INPUT}}

## Target length
4-5 sentences.

## Output
Plain prose only. No headers, no lists, no enumeration.
