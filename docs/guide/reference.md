# Pointers & where to read more

This guide is the *digested* view. When you need the authoritative,
always-current detail, these are the sources — and the guide defers to them
whenever they disagree with it.

## The durable sources of truth

- **`CLAUDE.md`** (repo root) — the operational reference. Commands, the full
  MCP tool list with signatures, the recipient-name convention, the chassis
  surface (what to mail where), the pre-flight and CI workflow. Loaded into
  every agent session. If the guide and `CLAUDE.md` disagree about a command
  or a tool signature, `CLAUDE.md` wins. For the local verification workflow
  — `scripts/preflight.sh`, `scripts/attest.sh`, and how to relocate their
  scratch onto a larger volume — see [Local verification](local-verification.md).
- **The ADR log** (`docs/adr/NNNN-*.md`) — the decision record, in the order
  decisions were made. Every claim in this guide about *why* a subsystem is
  shaped a certain way is digesting one or more ADRs; the citation is in the
  text. Read the ADR for the authoritative reasoning and the alternatives that
  were rejected. Start a new one from `docs/adr/TEMPLATE.md`.
- **The code** — the final authority. If a page names a file, function, or
  kind that no longer exists, the code is right and the page is stale. Fix the
  page.

## Live introspection (ask the running engine)

Much of what you'd want from "reference" is better asked of a live engine than
read from a doc, because the engine can't drift from itself:

- `describe_kinds` — the static substrate kind vocabulary with full schemas.
- `describe_component(engine_id, component)` — a loaded component's handler
  kinds and per-handler docs, addressed by lineage name (or a `mbx-` id).
- `describe_transforms` — the native `#[transform]` functions linked at build
  time.
- `actor_logs(engine_id, mailbox_name)` — recent entries from one actor's log
  ring.

Reach for these before assuming a doc is current.

## How the docs divide up

| Source | Question it answers | Form |
|---|---|---|
| This guide | "How does the system work, and how do I build with it?" | Digested narrative |
| `CLAUDE.md` | "What's the command / tool / convention right now?" | Terse reference |
| ADRs | "Why was this decided, and what was rejected?" | Decision record |
| Live introspection | "What does *this* engine actually have right now?" | Queried from the engine |
| The code | "What is true?" | The authority |

## Contributing to this guide

The guide builds with [mdBook](https://rust-lang.github.io/mdBook/). Source is
under `docs/guide/`; `docs/book.toml` is the config. Build locally with
`mdbook build docs` (output in `docs/book/`, gitignored) or `mdbook serve docs`
to preview. On merge to `main`, CI rebuilds and publishes to GitHub Pages.

When you add or change a callable surface, add or update the matching recipe in
the same change — the tutorial is the API sanity check, and writing it during
design is how a mis-shaped API gets caught before it freezes.
