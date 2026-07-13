---
name: scope-headless
description: Headless variant of /scope for a one-shot GitHub Actions runner. Executes ../scope/SKILL.md verbatim for the Define → Design → Plan process and judgment; overrides only the interaction surface — bounces become ask-and-park, the terminal Plan wait becomes end-turn — per ../headless/protocol.md.
---

# /scope-headless — headless scoping wrapper

This wraps `/scope` for a headless agent running one-shot on an ephemeral GitHub Actions runner. It carries no process of its own.

Execute `../scope/SKILL.md` verbatim — the full Define → Design → Plan walk, the phase-label reconcile, the body-editing mechanics, the grounding and API-budget discipline, every judgment call. Where the original touches the interaction surface, `../headless/protocol.md` governs. An instruction is overridden if and only if it appears in the Overrides table below, cited by the original's anchor; everything else is the original's, unchanged. An improvement to `/scope` is live here the moment it merges, because this wrapper only references it.

Before any process step, run the protocol's [re-entrancy-first](../headless/protocol.md#re-entrancy-first) guard: read the issue's current `phase:*` label and check for an unanswered `agent:awaiting-answer` park, then post a start-of-work comment with the run link and begin the original at the phase the observed state implies.

## Overrides

| Original anchor | Interactive behavior | Headless override |
|-----------------|---------------------|-------------------|
| [`### Define`](../scope/SKILL.md#define) Bounce | Self-bounce with a comment asking the specific clarifying question, then the user re-invokes | [ask-and-park](../headless/protocol.md#ask-and-park) — post the structured question comment, apply `agent:awaiting-answer` + `agent:park:scope`, exit 0 |
| [`### Design`](../scope/SKILL.md#design) Bounce | Self-bounce on a tied value-judgment only the user can make | [ask-and-park](../headless/protocol.md#ask-and-park) |
| [`### Design`](../scope/SKILL.md#design) ADR drafting | Scaffold an ADR on a branch and open a PR; the issue is not `/approve`-eligible until it merges | [ask-and-park](../headless/protocol.md#ask-and-park) — a headless scoper cuts no branch; when the chosen approach is load-bearing, park the ADR decision to the owner rather than drafting it |
| [`## Comments`](../scope/SKILL.md#comments) self-bounce question/blocker | Prose comment addressed to a human | [ask-and-park](../headless/protocol.md#ask-and-park) — the same content, posted in the machine-parseable park shape |
| Terminal "Stops at Plan, awaiting `/approve`" and [`## Restart and resume semantics`](../scope/SKILL.md#restart-and-resume-semantics) "the user re-invokes" | Stop at Plan and wait for the operator to re-run | [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) — leave the issue at `phase:plan`, end the turn; re-dispatch resumes the flow |

Everything the table does not cite — the Define/Design/Plan process, the Grounding, Side findings, Body editing mechanics, Phase label reconcile, GitHub API budget, size and model routing — is `/scope`'s, verbatim. A headless scoper still stamps `size:*` and `model:*` at Plan exactly as the original specifies.
