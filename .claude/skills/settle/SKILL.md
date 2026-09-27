---
name: settle
description: "Answer the open questions an Aether issue plan, ADR draft, or agent hand-back left for the owner by applying the design rulebook, with a certainty level on each answer. Apply only Settled answers; route Leaning and Open ones to the owner. Run after /scope or /adr, never inside them."
---

# /settle — answer open questions from the design rulebook

Support an issue number, a pull request number (an ADR draft), or `--from <file>` holding pasted questions such as an agent's "decisions for the owner". `/scope` and `/adr` write their open questions as they find them; this skill is the separate second pass that answers them. The style source is [the design rulebook](../../../docs/guide/contributing/design-rules.md) and the precedent it cites, nothing else.

## 1. Collect

Gather every open question from the target, numbered:

- an ADR draft's `## Open questions` section, and any "open question" named inline in its decision text;
- an issue's Plan sections, where a decision is left for the owner;
- a pasted hand-back's questions or listed decisions.

With none, report that and stop.

## 2. Ground each premise

For each question, name the facts it rests on — what code does today, what an ADR already decides — and read them at current `origin/main`, citing `file:line` or `ADR-NNNN §N`. A question whose premise the reading contradicts gets the verdict **Premise**: state the correction and do not answer the question. Never judge a question from recall.

## 3. Apply the rulebook

Read the rulebook whole. For each question, list the candidate answers and, for each, the rules that bear on it. Follow each rule's Settled citations into the ADRs and issues they name when the rule's text leaves the case unclear. Give one answer and one certainty:

- **Settled** — one rule decides the question by its own terms, and no rule pulls the other way. Cite the rule's heading and one of its Settled citations.
- **Leaning** — the rules decide it only by analogy to a past ruling, or one rule decides it while another pulls against. Name the analogy or the strongest counter-rule.
- **Open** — no rule applies, or rules conflict evenly. State the tradeoff in one line and the options it leaves.

Rules are immutable and carry stable ids (`R-NNNN`). Before applying a cited rule, follow its `Superseded by` forward references to the head; the head is the rule in force, and a superseded rule is never applied. Every answer cites rules by id: Settled cites the rule that decides it, Leaning cites the rule it follows and the counter-rule or ruling it rests on, and Open cites the rules in conflict or states that none applies. An answer that cites no rule is not an answer. Certainty comes from the rulebook and its precedent only. Do not raise it on your own taste, and do not invent a rule to reach Settled.

## 4. Report

Lead with one table, then nothing else a reader must parse:

| # | Question | Answer | Certainty | Basis |
|---|---|---|---|---|

Basis is the rule heading plus a citation, the premise correction, or the one-line tradeoff.

## 5. Apply Settled answers

Write each Settled answer into the target, in place, with its citation: the decision text names the rule it follows by id, as a link to its rulebook entry (`[R-NNNN](../guide/contributing/design-rules.md#r-nnnn)` from `docs/adr/`), so a reader can check the reasoning without asking.

- **ADR draft:** a Proposed ADR is edited in place, with no amendment notes. Write the decision into the section it belongs to and remove the question; delete `## Open questions` when it empties. Commit and plain-push to the draft's branch; the owner still reviews the ADR before it lands.
- **Issue plan:** edit through `/scope` mechanics: a guarded body write that re-reads and byte-compares before the write, then recompute the digest. A changed digest voids any earlier approval, so report the new digest for `/approve`.

Leave Leaning, Open, and Premise questions in place for the owner. When a permission or classifier refusal blocks a write, stop and give the owner the exact command; never route around it.

## 6. Record the owner's rulings

When the owner answers a Leaning or Open question, or overturns a Settled answer:

1. Apply the answer to the target as in step 5, citing the rule it follows, or the new rule that step 2 adds.
2. Record it in the rulebook in a pull request. The rulebook is append-only: append the ruling to the rulings log, citing the rule ids it follows, and append a new rule when none covers it.
3. An overturned Settled answer means the rule is wrong or incomplete. Never edit a rule's text: append a new rule that states the corrected rule, and add a `Superseded by R-NNNN` line to the old one, the only edit an existing rule ever receives.

The rulebook is published: rules are impersonal, with no names and no quoted words.

## Never

- Settle a question during `/scope` or `/adr`.
- Answer a question whose premise you have not read in code.
- Apply a Leaning or Open answer, or edit a section the questions do not touch.
