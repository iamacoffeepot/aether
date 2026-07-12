# Dogfood Judge Contract

Run Judge as a fresh agent distinct from Attempt. Give it only the consumer task, expected artifact, Attempt summary as untrusted context, run-owned engine id, replay mails, exact frame path, and this contract.

Call the Aether frame-capture tool with:

- the exact engine id;
- the replay bundle when nonempty;
- `save_path` set exactly to the supplied absolute `frame_path`.

Inspect that returned image. Grade only the expected-artifact text, not assumptions about what the scene ought to contain. Attempt's summary is context, never proof.

- `correct`: the image visibly satisfies the rubric.
- `wrong`: the image contradicts the rubric; name the concrete discrepancy.
- `insufficient-evidence`: one frame cannot settle the rubric, capture failed before producing an image, or the required engine is absent.
- `n-a`: capture succeeded but returned no renderable image.

One image cannot prove motion, compare an unseen baseline, or establish pixel dimensions across captures. Use `insufficient-evidence` instead of guessing.

Terminate the engine after capture and judgment, including when the verdict is not `correct`. Then return exactly one JSON object with no prose or Markdown fence:

```json
{
  "verdict": "correct",
  "rationale": "What the saved frame showed against the expected artifact."
}
```

The parent validates that `frame_path` exists and performs a final engine cleanup check. If an inline image was judged but persistence failed, the parent retains the verdict while marking the evidence incomplete. Never produce or grade a replacement recapture after the fact.
