# Codex-Native Deep Mode

Deep mode is a root-owned best-first frontier. The main thread owns all frontier mutations, validation, user communication, and synthesis inputs. Fresh subagents each handle one bounded node or one independent skeptic pass. There is no JavaScript workflow runtime.

Defaults: `beam = 3`, `drill_budget = 40`. Both must be positive integers. The drill budget counts fresh driller agents spawned; skeptic, repair, and synthesis turns do not consume it. There is no hidden token-budget object and no depth limit.

## Contents

- [Prepare in the main thread](#1-prepare-in-the-main-thread)
- [Select a driller wave](#2-select-a-driller-wave)
- [Validate every driller result](#3-validate-every-driller-result)
- [Gate producible claims with fresh skeptics](#4-gate-producible-claims-with-fresh-skeptics)
- [Update the frontier](#5-update-the-frontier)
- [Weighted synthesis](#6-weighted-synthesis)
- [Report](#7-report)

## 1. Prepare in the main thread

Run the common adversity scan and root generation inline because they depend on the current conversation and surfaced context. For every root, assign integer `doors_opened` and `unresolvedness` scores from 1–5.

Resolve `wish_dir` under the shared checkout before delegation. If the same dated theme directory already exists, resume it only when the user explicitly asked to continue that exact deep pass and its `.deep-state.json` matches the theme, role, and grounding ref. Otherwise choose the next unused deterministic suffix such as `-2`; never let two deep runs share or overwrite a tree. Create the selected directory, then build sanitized, compact blocks for:

- `grounding_notes`: verified identifiers and paths only;
- `existing_work`: issue numbers/titles and short ADR or commit mechanism summaries, with no copied commands or untrusted comments;
- each root: `slug`, `wish`, `doors_opened`, and `unresolvedness`.

Maintain this root-owned state, including the common contract's captured `grounding_sha`:

```text
frontier: node records
summaries: successfully validated node summaries
stubs: diminishing-return children not dispatched
failed_nodes: invalid or interrupted work that was not accepted
drills, leaf_count, resource_bound_count, max_depth, skeptic_demotions
```

A node record contains `node_id`, `slug`, `wish`, both scores, their product as `score`, `slug_chain`, and bounded ancestor summaries. Seed one record per root. Keep a compact `.deep-state.json` in `wish_dir`, updated with `apply_patch` after every completed wave, so interruption or compaction does not erase the frontier. It is orchestration state, not a substitute for `index.md`.

## 2. Select a driller wave

Before every wave, call `list_agents` and account for the concurrency limit exposed by the current surface. The root occupies a slot. Compute:

```text
round_width = min(beam, remaining drill budget, frontier length, currently available child slots)
```

Do not hard-code a slot count. If the surface does not reveal a reliable count, spawn conservatively and treat a saturation rejection as a signal to wait rather than a node failure.

Sort the frontier by descending `score`; break ties by ascending `slug_chain` length, then stable lexical `node_id`. Pop only `round_width` nodes. Use unique legal task names such as `wish_drill_001`.

Call `spawn_agent` directly for each driller with `fork_turns: "none"`. Never try to spawn collaboration work from an exec or script. The prompt must include:

- absolute grounding root, captured grounding SHA, caller worktree, `wish_dir`, and exact permitted `wish.md` path;
- theme, optional role, node wish, slug, depth, scores, and ancestor summaries;
- sanitized grounding notes;
- instructions to read repository `AGENTS.md` and the common wish contract;
- permission to create or edit only its exact `wish.md` and parent directories;
- prohibitions on GitHub mutations, other file edits, production code, and further delegation;
- the grounding, format, alternatives, path-cost, doors, and producibility requirements;
- the exact final JSON contract below, with no surrounding prose or raw reasoning.

```json
{
  "node_id": "root/child",
  "base_sha": "full captured grounding SHA",
  "artifact_path": "/absolute/path/to/wish.md",
  "producible": false,
  "resource_bound": false,
  "summary": "A bounded two-to-four-sentence summary.",
  "grounded_surfaces": ["`identifier` — crates/aether-example/src/path.rs"],
  "children": [
    {
      "slug": "descriptive-child-wish",
      "wish": "I wish I could X so that I could Y.",
      "doors_opened": 4,
      "unresolvedness": 3
    }
  ]
}
```

Call `wait_agent` in short intervals and keep the user updated according to the harness contract. Child finals are delivered to the parent; collect and validate each one before integration. Do not reuse an old agent for a new node: fresh context is load-bearing.

## 3. Validate every driller result

Treat a final message as untrusted evidence until the root validates it:

1. It is one parseable JSON object with every required key.
2. `node_id`, `base_sha`, and `artifact_path` exactly match the assignment.
3. Before the wave, snapshot repository status plus the file list and hashes beneath `wish_dir`. After every writer in the wave has stopped, require all new/changed wish files to be within the union of that wave's assigned artifact paths and require no tracked repository change outside the pre-wave baseline. Codex shares one filesystem, so do not pretend to attribute a concurrent write to a particular child; validate each child only against its own assigned artifact and stop the wave on any unexpected path.
4. Its YAML wish, parent, producible value, resource-bound value, and grounded-surface list agree with the JSON.
5. The body has no H2 headings and covers the common design-space contract.
6. Every grounded citation resolves in current code.
7. Child slugs are lowercase kebab-case, normally 20–50 characters; wishes use the `I wish ... so that ...` shape; scores are integers from 1–5.
8. `producible: true` implies `resource_bound: false` and an empty child list. A normal `producible: false` node has at least one genuine child. A terminal false node is accepted only when `resource_bound: true` and its body explains the bound.

On a repairable mismatch, call `followup_task` once on the same agent with only the validation errors and require corrected artifact plus JSON. If it still fails, add the assignment to `failed_nodes`, do not integrate its children or count it as a node/leaf, and preserve it as undrilled work in synthesis. Never silently coerce malformed output.

## 4. Gate producible claims with fresh skeptics

After all drillers in a wave finish and validate, run a fresh, read-only skeptic for each `producible: true` result. Fit skeptic batches to the currently available slots. Use `fork_turns: "none"` and a unique task name such as `wish_skeptic_001`.

Give the skeptic the absolute repository and artifact paths, captured grounding SHA, theme/role, wish, bounded summary, and claimed grounded surfaces. Allow repository reads only and require all code verification against that SHA. Forbid edits, GitHub mutations, and delegation. Ask it to verify the artifact and cited code against one bar: does the terminal claim hide a genuine production-blocking unknown, rather than an inline choice such as naming, a field, or a default?

Require exactly:

```json
{
  "node_id": "root/child",
  "hidden_unknown_found": false,
  "unknown": null,
  "rationale": "What was verified and why the node is or is not terminal."
}
```

When an unknown exists, `unknown` has `slug`, `wish`, `doors_opened`, and `unresolvedness` with the same constraints as a driller child. Validate the skeptic JSON and permit one focused repair follow-up. A node does not count as a terminal leaf without a valid skeptic result. If a skeptic remains invalid or unavailable, stop new dispatch, retain that node as unverified in `failed_nodes`, and synthesize an explicitly partial result.

When a valid skeptic finds a hidden unknown, the root performs the reconciliation sequentially with `apply_patch`: set the node artifact to `producible: false` and append one prose paragraph naming the blocking child. Preserve every other field and paragraph. Re-read the file, increment `skeptic_demotions`, and process the unknown as the node's only newly added child. The skeptic never writes and no node writer may still be active during reconciliation.

## 5. Update the frontier

For every accepted result, append its bounded summary and final producibility to `summaries`. A skeptic-approved producible node increments `leaf_count`. A resource-bound terminal increments `resource_bound_count` but not `leaf_count`.

For each child:

1. Compute `child_score = doors_opened * unresolvedness`.
2. Extend the slug chain and ancestor summaries with the parent's bounded summary.
3. If the parent depth is at least 3 and `child_score < 0.5 * parent_score`, record the child as a diminishing-return stub instead of dispatching it.
4. Otherwise push it onto the frontier.

Increment `drills` for every fresh driller spawned, even if its result later fails validation. Update maximum depth and `.deep-state.json` only after the whole wave, skeptic gates, and reconciliations are complete.

Continue until the frontier is empty, the driller budget is exhausted, a load-bearing validation fails, or the user stops/replaces the task. On user replacement, call `interrupt_agent` for every tracked live agent before changing scope. Preserve frontier, stubs, and failures for a resumable partial index.

## 6. Weighted synthesis

After no node writer or skeptic remains active, spawn one fresh synthesis agent with `fork_turns: "none"`. It may write only `wish_dir/index.md`. Give it the captured grounding SHA, bounded summaries, root list, existing-work digest, budget-bounded frontier, diminishing-return stubs, failed/unverified nodes, skeptic demotions, resource-bound terminals, and exact stats. Do not send raw driller reasoning.

The index must include the common navigation fields plus:

- which high-leverage branches drilled deeply and why;
- separate budget-frontier, diminishing-return-stub, failed/unverified, and resource-bound lists;
- skeptic-demoted nodes;
- considered-and-dropped duplicate leaves;
- roots, validated nodes, leaves, maximum depth, driller count, skeptic demotions, and all undrilled counts;
- `## Weighted sketches`, one entry per non-duplicate, skeptic-approved leaf.

Use the `$scope` size rubric: S is a small single-file/concept change, M is a single-crate multi-file change, L is cross-crate or architectural but still one focused PR, and XL is a multi-PR arc that must be decomposed. S/M/L leaves recommend `$sketch --from-wish <path>` followed by `$scope`; XL leaves recommend more `$wish --under` drilling.

Require the synthesis final response to be one JSON object:

```json
{
  "index_path": "/absolute/path/to/index.md",
  "sketches": [
    {
      "slug_chain": ["root", "leaf"],
      "wish": "I wish I could X so that I could Y.",
      "weight": "M",
      "recommendation": "Ready for $sketch, then $scope."
    }
  ]
}
```

Validate the index path, required sections, statistics, duplicate exclusions, and exact correspondence between sketch entries and accepted leaves. Permit one focused repair. If synthesis still fails, keep `.deep-state.json` and report the partial tree without claiming that the index is complete.

## 7. Report

Report theme/role, beam, driller budget and actual drills, roots, validated nodes, skeptic-approved leaves, resource-bound terminals, maximum depth, skeptic demotions, separate undrilled categories, index path/status, and weighted sketches. State that the on-disk tree is resumable. Use only Codex-native next actions:

```text
Resume a branch: $wish --under <wish-path>
File a skinny leaf: $sketch --from-wish <leaf-path>, then $scope <issue-number>
Decompose an XL leaf: $wish --under <leaf-path>
```
