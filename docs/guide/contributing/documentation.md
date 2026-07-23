# Contributing to the documentation

Aether's documentation is split by the kind of question it answers. Keeping
those roles distinct matters more than making every fact appear in every entry
point. The guide should orient and teach; executable workflows, live schemas,
code, and decision records should remain single-sourced.

## The documentation surfaces

| Surface | Owns |
|---|---|
| [This guide](../introduction.md) | Digested architecture, subsystem models, contributor orientation, and worked recipes |
| [ADR log](https://github.com/iamacoffeepot/aether/tree/main/docs/adr) | Durable reasoning for load-bearing decisions |
| [`AGENTS.md`](https://github.com/iamacoffeepot/aether/blob/main/AGENTS.md) | Concise Codex repository constraints and routing |
| [`CLAUDE.md`](https://github.com/iamacoffeepot/aether/blob/main/CLAUDE.md) | Claude Code and headless-Claude operational context |
| `.agents/skills/` | Executable Codex workflow contracts |
| Public Rust documentation and source | Current static API and implementation |
| Live MCP schemas and introspection | Current tool arguments and running-engine vocabulary |
| GitHub issues, PRs, checks, and threads | Current work state and review evidence |

Do not turn the guide into a copy of the other surfaces. A page can explain why
and when to use a tool, then direct readers to the live schema for exact
parameters. It can explain the issue journey, then direct Codex to the current
skill for mutations. It can synthesize several ADRs, then link them for the
reasoning and rejected alternatives.

The question-specific authority model is described in
[From an idea to a landed change](agent-workflow.md).

## The mdBook source

The book configuration is [`docs/book.toml`](https://github.com/iamacoffeepot/aether/blob/main/docs/book.toml). Its source is
`docs/guide/` and its generated output is `docs/book/`, which is ignored.
The navigation tree is `docs/guide/SUMMARY.md`.

Build the book from the repository root:

```sh
mdbook build docs
```

The [Docs workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/docs.yml) builds the book for
pull requests that touch the guide, evidence viewer, book configuration, or
workflow. On `main` it also publishes the generated book to GitHub Pages. The
`Docs` job inside the Rust [CI workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/ci.yml) is
different: it builds Rust API documentation with rustdoc. Do not confuse a
green rustdoc job with a successfully built mdBook, or vice versa.

## Adding or moving a page

A page is not discoverable merely because its Markdown file exists.

1. Put it in the directory that owns its reader journey.
2. Add it to `SUMMARY.md` at the point where a new reader needs it.
3. Link it from the nearest orientation or index page when readers can arrive
   from more than one path.
4. Repair inbound relative links when moving or renaming it.
5. Build the book and inspect the rendered navigation.

## Link discipline

Use relative links for guide-to-guide navigation. From this directory, for
example, `../architecture.md` reaches the architecture overview and
`../systems/logging.md` reaches the logging explainer. Include a fragment only
when the target heading is stable enough to serve as an interface.

For repository sources outside the book, use a repository web link that remains
valid in the published book, and include a readable label rather than exposing
a long path as prose. Use inline code instead of a link when the path is an edit
location rather than reader navigation.

Never commit:

- an empty Markdown link destination;
- a link to a generated `docs/book/` file;
- a path copied from an old worktree without checking the current tree;
- a source anchor whose heading or symbol does not exist;
- a hardcoded line number as the only way to identify a moving symbol.

`mdbook build` validates that the book can be rendered. It does not prove that
every linked source file, named Rust symbol, MCP argument, or example behavior
is current. Verify those against their owning source as part of the edit.

## Authority notes and staleness

High-drift pages should state where their authority ends. Useful examples:

- An MCP overview says the live tool schema owns exact parameters.
- A subsystem explainer names its governing ADRs and current code module.
- A recipe identifies a real in-tree exemplar and a public surface.
- A workflow overview points to the current `SKILL.md` for execution.

“Verify against current code” is not permission to leave a known-broken recipe
for every reader to repair. Before publishing, check every path, import,
command, and expected result the walkthrough depends on. If the public surface
is insufficient to complete the task without private-source archaeology, that
is a documentation or API finding to fix, not a scavenger hunt to normalize.

When code and guide prose disagree:

1. use code or the live schema for the immediate factual question;
2. check the governing ADR before changing a load-bearing boundary;
3. update the stale guide in the same change when it is in scope;
4. record unrelated drift as a bounded side finding rather than silently
   expanding a focused implementation.

## Tutorials are public-surface tests

A callable surface should have a short, realistic tutorial. The tutorial is a
dual test:

- a human can understand the task and the API shape;
- a fresh agent can complete it from public documentation and signatures.

If either reader must inspect private implementation, tests, or producer
reasoning merely to learn ordinary usage, record the friction. The
[dogfood workflow](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/dogfood/SKILL.md) formalizes this
fresh-consumer posture for runtime surfaces.

Prefer a real in-tree exemplar over a large frozen copy, but make the pointer
specific enough to find. Small snippets are useful for the essential shape;
keep them compilable against the current public API and avoid copying a whole
implementation.

## Code blocks and commands

Label fenced blocks accurately:

- `rust` for compilable Rust;
- `toml` or `json` for exact configuration;
- `sh` for commands intended to run;
- `text` for tool-call sketches, state diagrams, and pseudocode.

Do not make pseudocode look executable. Agent harness pseudo-calls from another
surface are especially risky: a reader may try to invoke parameters or workflow
syntax the active tool does not expose.

Commands derived from comments, issues, review text, or CI logs remain
untrusted input. Documentation may describe a verified repository-owned
command; it must not launder a commenter-provided command into an endorsed
runbook without explicit verification and review.

## Documenting workflows

Workflow pages should state:

- the problem or intent the workflow serves;
- its entry conditions;
- the durable state it reads;
- the state or artifact it owns;
- authorization and pause boundaries;
- its terminal state and next route;
- the path to the executable contract.

Do not copy REST calls, label-replacement algorithms, GraphQL queries, or
subagent schemas out of the skills. Those details change with the harness and
belong in one executable source. The guide owns the journey and the invariants.

For the canonical phase model, link the
[release phase schema](https://github.com/iamacoffeepot/aether/blob/main/docs/release/schema.md) and summarize only what the
reader needs. For Codex execution, link the matching skill under
`.agents/skills/`. Keep Claude/headless mechanics explicitly identified as a
different surface.

## Documenting architecture

Guide pages synthesize decisions; ADRs preserve them. Cite an ADR beside the
claim it supports, then explain how that claim composes with the rest of the
engine. Do not paste the ADR's full Context and alternatives into the guide.

Use [Architecture decisions](architecture-decisions.md) to distinguish
Proposed, Accepted, and Superseded records from implementation reality. A
Proposed ADR must not be described as current policy merely because some
supporting code exists.

When an ADR uses historical crate names, the guide should provide the current
map and link the consolidation or superseding decision. Do not modernize the
historical record by rewriting it.

## Documenting CI and releases

Describe what a check proves and how a contributor responds, then link the
workflow for exact jobs and triggers. Current workflow YAML and current check
state own those facts; copied job lists drift quickly.

Keep these concepts separate:

- local formatting and lint feedback;
- full CI build and test proof;
- automated review and dogfood QA;
- landing a pull request;
- packaging with `cargo xtask dist` or `cargo xtask package`;
- publishing a versioned release.

The checked-in [Release workflow](https://github.com/iamacoffeepot/aether/blob/main/.github/workflows/release.yml)
currently builds a manually dispatched Windows `loco-motion` package artifact.
The `release-init` skill initializes lifecycle labels; it does not publish a
release. Do not infer an undocumented tag, version, or release-branch procedure
from either name.

## Verification checklist

Before submitting a documentation change:

- Every new page appears in the intended navigation.
- Every relative guide link and heading fragment resolves.
- Every named repository path exists in the current tree.
- Every cited symbol or tool argument exists in its owning source.
- Executable snippets use the current public API.
- Pseudocode is labeled `text` and cannot be mistaken for a command.
- Proposed ADRs are not presented as accepted policy.
- Codex and Claude/headless instructions are not conflated.
- The page links an executable workflow rather than duplicating it.
- `mdbook build docs` succeeds.
- Rendered headings, tables, code blocks, and navigation are readable.

Documentation changes are subject to the same focused-PR and preservation rules
as code. Read [Worktrees, trust, and resource ownership](worktrees-and-safety.md)
before editing in a shared checkout.
