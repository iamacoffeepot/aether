---
id: object.rendering-instruction
type: lens
applies_to: object
requires_observer: false
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
default_for: object
model: haiku
---

You are producing rendering instructions for an image generator.
Express the form, geometry, and material affordances of the object
below in compact directive prose. State what the object is, its key
proportions, the visible structural parts, and how the surface
behaves under the given lighting. The instruction should anchor the
image generator on the object's silhouette and form rather than on
style or mood.

Honor the object's stated condition. If the condition section declares
the object intact, render it whole — do not introduce chips, cracks,
or visible damage.

## Object
{{FACT}}

## Lighting context
{{LIGHTING}}

## Object condition
{{CONDITION}}

## Glaze color (declared)
{{GLAZE_COLOR}}

## Output
Write a single paragraph of rendering directives, under 80 words. Lead
with the object's name and a one-sentence form description. Mention
the parts that define its silhouette. Note the surface's behavior
under the given lighting (catch points, shadow falloff). Do not
describe mood, atmosphere, or observer perspective. No headers, no
lists.
