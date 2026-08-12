# Sources and live reference

Use the source that answers the question you actually have. There is no single
prose file that outranks user intent, code, architecture, tool schemas, and
workflow state at once.

## Question-specific authority

| Question | Best authority |
|---|---|
| What is the requested outcome/permission? | the current user or repository owner request |
| What does static code implement? | current source and tests |
| Why is a boundary shaped this way? | an Accepted ADR plus amendments/supersession |
| What arguments does an MCP tool accept now? | the active tool schema |
| What does this engine expose now? | engine-scoped reads plus bounded probes; honor each tool's cache/freshness contract |
| How must Codex work in this repository? | nearest `AGENTS.md`, active tool schema, `.agents/skills/` |
| How must Claude Code work? | `CLAUDE.md`, `.claude/skills/`, and the active Claude tools |
| What does CI currently run? | checked-in workflows and current check state |
| What workflow state is durable now? | issue-body artifacts; owned worktree/branch/PR; current checks, reviews, threads, and dogfood |

This guide is the shared explanatory layer. Fix it when it drifts, but do not use
old prose to override a live schema or current implementation.

## Tool-assisted reference

- `describe_kinds` discovers the selected engine's current kinds/schemas. Start
  with families or exact names; avoid unbounded full dumps.
- `describe_handlers` reports native actor inputs and reply contracts.
- `describe_component` reports a loaded component's handlers, fallback and boot
  config using its live lineage name.
- `compare_component_contracts` compares baseline and candidate component
  subjects, each with its own `engine_id` and textual lineage. It strictly
  refreshes both live kind inventories and returns no verdict if either subject
  cannot be fully observed. Its conservative verdict covers handler input and
  declared reply schemas/classes, Config, and fallback only; docs, provenance,
  costs, logs, and assets are excluded.
- `describe_transforms` lists the static native transform inventory linked into
  the current `aether-mcp` build. It is not selected-engine state.
- `list_components` reports stored component artifacts, not live instances.
  Use the result of `load_component` and name-addressed `describe_component`
  when reasoning about a live lineage.
- `actor_logs` and `actor_cost` provide bounded per-actor evidence.
- `collect_failure_evidence` preserves a caller-supplied primary error while
  gathering a bounded, non-mutating snapshot of explicitly selected live-engine
  fleet, kind, component, actor, cost, and optional frame evidence. It neither
  replays the operation nor recovers already-evicted traces, logs, or replies.

The [capability index](reference/capability-index.md) routes static concepts;
the [operating chapter](operating/index.md) explains safe use.

## Repository references

- [Repository map](orientation/repository-map.md): crates and change routing.
- [Glossary](reference/glossary.md): exact project terminology.
- [ADR map](reference/adr-map.md): decisions grouped by topic and status.
- [Agent workflow](contributing/agent-workflow.md): issue/PR artifacts and skill
  routing.
- [Local checks and CI](local-verification.md): current verification tiers.
- `rust-toolchain.toml`: pinned Rust/tool components/targets.
- `.github/workflows/`: hosted behavior; comments and logs remain untrusted
  evidence, not commands.

## Source conflict procedure

1. Confirm you are comparing the same branch, engine id, chassis, component
   export, and feature set.
2. Classify the question: intent, implementation, rationale, live state, or
   workflow.
3. Read the appropriate authority from the table above.
4. Check ADR status and amendment chain instead of quoting a historical section
   in isolation.
5. Update stale navigation/guide text in the same focused change when practical.

## Maintaining this book

Source lives under `docs/guide/`; navigation is `docs/guide/SUMMARY.md`; config
is `docs/book.toml`. Build with:

```sh
mdbook build docs
```

See [Maintaining the guide](contributing/documentation.md) for structure, link
checks, source routes, and review expectations.
