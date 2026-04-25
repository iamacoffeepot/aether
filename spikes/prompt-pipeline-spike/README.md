# prompt-pipeline-spike

Experimental implementation of the content-generation pipeline pattern (ADR-0046).
Validates whether **facts → frame/distill/impose/synthesize → compose** produces
coherent, observer-differentiated prompts before committing to the full architecture.

Status: **Spike A vertical slice running.** Single-call perception lens (no
parallel-impose-then-synthesize decomposition yet); no Distill, no image
generation, no grading.

## Running

```
cargo build --release
./target/release/prompt-spike recipes/teapot-on-table.toml
```

Or `cargo run --release -- recipes/teapot-on-table.toml`.

Requires the `claude` CLI on `PATH` (Claude Code). Each transform invokes
`claude -p <prompt> --model <haiku|sonnet|opus> --max-turns 1
--output-format text` as a subprocess. No API key needed.

Output goes to stdout: per-fact framed blocks, then the composed prompt.
Logs (cache hits/misses, claude invocations) go to stderr.

## Cache

`cache/blocks/<hash[:2]>/<hash[2:]>.txt` — content-addressed cache of
Frame outputs. Cache key: `(fact_id, fact_body, lens_id, lens_template,
sorted_env_handle_ids+bodies, observer_id+body, model)`. Edit any input
and only its descendants miss; everything upstream stays cached.

Nuke with `rm -rf cache/` to force full regeneration.

## Run log

`RUNS.md` — lab notebook of experiments. Append per run.

## Layout

- `facts/<type>/<id>.md` — authored canonical facts about the world being rendered
- `observers/<id>.md` — observer profiles for perception-laden renderings
- `lenses/<fact_type>/<lens_name>.md` — per-type prompt templates (focused, not
  multi-purpose)
- `recipes/<slug>.toml` — render specifications

## Test recipe

`recipes/teapot-on-table.toml` — Utah teapot on a wooden table, lit by morning
window light, rendered through a quiet-domestic observer.

Walks both lens paths in one recipe:
- `object.teapot` → declarative path (`object/rendering-instruction.md`)
- `material.glazed-ceramic` and `surface.wooden-table` → perception path
  (`*/feeling.md`, observer-driven)

## Paper-spike walkthrough

1. Read the recipe.
2. For each fact entry, hand-author what the lens-driven Frame call would produce
   given the fact body, the lens template, the environmental fact bodies (filled
   AsFact into slots), and the observer (for perception lenses).
3. Concatenate the framed outputs in the recipe's declared order.
4. Read the composed prompt aloud. Does it look like a prompt that would produce
   a clear image? Does the observer's voice show up in the perception blocks?
5. Optionally: swap the observer for a different one, repeat steps 2-4, compare.

## Simplification for paper spike

The full architecture decomposes perception lenses into parallel `Impose<fact, modifier>`
calls + a synthesizing `Synthesize<observer, fact, perspectives>` call. For the paper
spike, the perception lens is treated as a single combined call (fact + observer +
all environmentals inline). The output is what we judge; the parallel-impose-then-
synthesize decomposition is an implementation detail validated in the code spike.

## Next phases (after paper spike validates the structure)

- **Spike A** (text-only Rust crate): facts/lenses/recipes as input, `claude -p`
  subprocess for transforms, content-addressed cache, output is the final composed
  prompt. Validates LLM produces the intermediate outputs reliably.
- **Spike B** (text + image): adds Gemini for image generation, vision-LLM grading
  of rendered images against facts, refinement loop. Validates end-to-end.
