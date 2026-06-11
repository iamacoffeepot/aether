# Subsystem map

This page is the index to the per-subsystem explainers. Each subsystem gets a
dedicated page (under construction — see the nav) covering *why it exists*,
*what it does*, and *how to extend or reuse it*. Until a page lands, the row
below is the orientation: what it's for, what you mail it, and the ADRs that
govern it.

A note on the **maturity** column, because it changes how you should read a
page: *Stable* surfaces are safe to build on and document in full. *Settling*
surfaces have a stable outward face (what to mail, what it does) but internals
still in motion — the explainer documents the face and defers the guts to the
ADR until it lands. When in doubt, the ADR is authoritative.

## Mailbox addressing, first

Before the table: the rule that everything else assumes. `recipient_name`
names the **mailbox**; `kind_name` names the **payload shape**. They route
independently even when they share a prefix. Chassis-owned mailboxes live under
`aether.<name>` (`aether.render`, `aether.audio`, `aether.fs`, `aether.input`,
`aether.lifecycle`, `aether.window`, `aether.component`, `aether.handle`). A loaded wasm component
registers at `aether.component/aether.embedded:NAME` — use the full address that
`LoadResult.name` hands back. **Bare names** (`"camera"`, `"player"`) are not
registered and warn-drop.

## The subsystems

| Subsystem | What it's for | Mail it | ADRs | Maturity |
|---|---|---|---|---|
| **Mail & scheduling** | The universal interaction mechanism + the blob dispatcher that runs actors. | (the substrate itself; not a mailbox) | 0002, 0005, 0019, 0087 | Settling (scheduler internals) |
| **Kinds, schema & encoding** | Typed payloads that describe themselves on the wire. | — | 0005, 0031, 0032, 0064, 0065, 0091 | Stable |
| **Components & lifecycle** | Loading, replacing, and hot-reloading wasm actors. | `aether.component` — `load` / `drop` / `replace` | 0010, 0015, 0022, 0038, 0063, 0074 | Stable |
| **Input streams** | Key / mouse / window-size input interrupts on `aether.input`; the per-frame Tick is a lifecycle stage on `aether.lifecycle`. All publish-subscribe, keyed by `KindId`. | subscribe via `aether.input`; subscribe Tick via `aether.lifecycle` | 0021, 0068, 0082 | Stable |
| **Rendering & camera** | World-space geometry + a `view_proj` uniform; a camera publishes the matrix. | `aether.render` (`DrawTriangle`, `aether.camera`) | 0025, 0066, 0074 §7 | Stable |
| **Mesh authoring & the DSL** | Author meshes as DSL text, hot-load them, replay to the renderer. | `aether.mesh.load` to the mesh-viewer component | 0026, 0051, 0052, 0057 | Stable |
| **File I/O** | Namespaced read/write/delete/list — `save` / `assets` / `config`. | `aether.fs` — `read` / `write` / `delete` / `list` | 0041 | Stable |
| **Audio** | Fire-and-forget note on/off + master gain; built-in instruments. | `aether.audio` — `note_on` / `note_off` / `set_master_gain` | 0039 | Stable |
| **Window** (desktop) | Mode / title / focus; replies with the value actually applied. | `aether.window` — `set_mode` / `set_title` / `focus` | 0035 | Stable |
| **Tracing & settlement** | Watch a mail chain to exact completion; trace subtree returned to the agent. | via `send_mail_traced` | 0080, 0086, 0093, 0094 | Settling (eviction, #1048) |
| **Logging** | Per-actor log rings, queryable by mailbox name. | — (read via `actor_logs`) | 0077, 0081 | Stable |
| **Configuration** | Layered app config — derive + overlay + argv/env, dumped via `--config`. | — (boot-time + CLI) | 0090 | Settling (rollout across knobs) |
| **Handles** | Typed references to substrate-held values; a value travels by reference, not by bytes. | `aether.handle` — `publish` / `release` / `pin` / `describe` | 0045, 0048, 0049 | Stable; lightly exercised |
| **Computation DAG** | Large/async work as a graph of sink calls + transforms wired by handles. | via `submit_dag` / `dag_status` / `dag_cancel` | 0045, 0047, 0048, 0084 | Stable (pattern); lightly exercised |
| **HTTP egress** | Outbound network as a capability. | `aether.http` | 0043 | Stable |

## How to read an explainer (the shape each page follows)

When the per-subsystem pages land they'll each answer the same four questions,
in this order — so you can jump straight to the one you need:

1. **Why it exists.** The problem it solves and the alternative it rejected
   (this is the ADR's "context," digested).
2. **What it does.** The model — the mailboxes, the kinds, the reply contracts
   if any, the invariants.
3. **How to use it.** The operational surface from a caller's seat: what to
   mail, what comes back, the gotchas inline.
4. **How to extend or reuse it.** The seams — what you'd add a kind for, where
   a new capability plugs in, what a component can reuse.

That last question is the one this guide exists to answer, and it's where the
[recipes](recipes.md) take over with the worked, step-by-step version.
