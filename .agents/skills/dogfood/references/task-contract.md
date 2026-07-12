# Dogfood Task Contract

Return exactly one JSON object with no prose or Markdown fence:

```json
{
  "medium": "drive",
  "prompt": "Build a concrete consumer outcome that necessarily uses the surface under test.",
  "surfaceUnderTest": "public mail kinds, SDK API, or infrastructure API",
  "expectedArtifact": null
}
```

## Fields

- `medium`: `drive`, `author`, or `build-layer`.
- `prompt`: a concrete, accomplishable consumer job. It must be impossible to finish without the named new surface and must not leak producer file paths or implementation details.
- `surfaceUnderTest`: the public docs, signatures, mail kinds, SDK macros, or infrastructure API the consumer must use.
- `expectedArtifact`: a specific visual rubric when one frame can settle correctness; otherwise `null`.

Choose the medium by what the consumer must write:

- `drive`: write no crate; operate the running engine through public MCP mail/tool surfaces.
- `author`: create a guest wasm component against the public actor SDK and load or drive it.
- `build-layer`: create a scratch native consumer of public workspace crates, such as a capability, kind family, or infrastructure layer.

Avoid comparison rubrics that one image cannot settle. If the task requires before/after, motion, dimensions, or multiple states, either define evidence that can appear together in one frame or use `expectedArtifact: null`.

A newly generated `author` or `build-layer` task requires human approval before Attempt. A task from a valid scoped Dogfood brief or supplied explicitly by the user is already approved.
