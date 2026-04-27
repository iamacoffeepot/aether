# ADR-0058: Namespace chassis sinks under `aether.sink.*`

- **Status:** Accepted
- **Date:** 2026-04-27

## Context

Chassis-owned sinks live in the bare-name mailbox space: `render`,
`camera`, `audio`, `io`, `net`, and `handle` (ADR-0045's typed-handle
sink, missed by issue #265's enumeration). Any component loaded under
one of those names collides with the chassis sink.

PR 263 made that collision fail cleanly instead of silently wedging
the engine, and feedback memory `feedback_chassis_sink_name_conflicts`
encodes the rule for the operator's local Claude. Neither is a
permanent fix:

- Memory is per-operator, per-machine, and only surfaces if the
  harness recalls it.
- Every fresh agent session — every `/compact`, every cold-started
  Claude on this repo — starts naive about the reserved list. It
  will name a camera component `camera` because that is the natural
  name. The clean error message is found by hitting the wall, not
  by reasoning ahead of it.
- New chassis sinks added later (the ADR-0045 `handle` sink is the
  most recent example) widen the reserved set with no notification
  to anyone.

The forcing function is the in-repo agent harness, not a hypothetical
plugin system: Claude is the consumer, and naming collisions recur
across compactions.

There is a deeper asymmetry pulling in the same direction. Two
chassis-owned mailboxes are *already* namespaced: `aether.control`
(the control plane) and `hub.claude.broadcast` (the hub-side
fan-out sink). The bare names `render` / `camera` / `audio` / `io`
/ `net` / `handle` are the only chassis-owned mailboxes outside a
namespace, and they are exactly the ones that collide.

## Decision

Move all chassis-owned sinks into the `aether.sink.*` namespace.
The migration list is fixed:

| Bare name (today) | Namespaced name (after) |
| ----------------- | ----------------------- |
| `render`          | `aether.sink.render`    |
| `camera`          | `aether.sink.camera`    |
| `audio`           | `aether.sink.audio`     |
| `io`              | `aether.sink.io`        |
| `net`             | `aether.sink.net`       |
| `handle`          | `aether.sink.handle`    |

The bare names then belong to user-space components and are free for
the obvious naming choice (`load_component(... name: "camera")` for
a camera component) without colliding with anything the chassis owns.

`aether.control` and `hub.claude.broadcast` keep their current names;
they are already namespaced and the well-known constants are baked
into the SDK, MCP harness, and on-the-wire `Hello` handshake.

This is a hard cutover in a single PR (no alias period). All in-tree
callers update at once: ~72 references across substrate binaries, the
SDK, components, demos, tests, and CLAUDE.md. Any saved agent
transcript that mailed bare names becomes stale, which is fine — the
clean-error path from PR 263 still surfaces a useful diagnostic if
someone replays an old send.

## Consequences

- **Bare names become user-space.** Loading a component as `camera`
  or `render` no longer errors; the name space is genuinely free.
  This is the goal.
- **Verbosity tax.** `recipient_name: "aether.sink.render"` is
  longer than `"render"`. Felt on every send. Acceptable: agent code
  is the dominant consumer and pays no real cost; humans pay it once
  per call site.
- **Mailbox names are visibly distinct from kind names in logs.**
  `aether.sink.audio` mailbox vs `aether.audio.note_on` kind reads
  unambiguously. The `aether.<subsystem>` alternative below would
  collapse `aether.camera` to mean both a kind and a mailbox, which
  works in code (different ID-domain prefixes per ADR-0029/0030)
  but is ambiguous to read.
- **CLAUDE.md churn.** The "Recipient-name convention" paragraph
  inverts: short names *don't* match kind namespaces; the
  `aether.sink.` prefix is the rule. ADR-0033 / ADR-0039 / ADR-0041
  / ADR-0043 / ADR-0045 prose stays correct because each ADR talks
  about its sink by its current bare name; updates are local to
  examples and the convention paragraph.
- **Tests update.** Substrate boot tests, hub MCP integration tests,
  and component examples address sinks by name; all move at once.
- **No saved-state migration.** Mailbox ids are name-derived
  (FNV-1a 64 with `MAILBOX_DOMAIN`, ADR-0029) but they are never
  persisted — a component addresses a sink at send time, and the
  hub resolves descriptors at handshake time. Renaming the sinks
  recomputes the ids; nothing on disk references the old ids.
- **Forecloses option 4.** This decision treats the structural
  asymmetry as the root cause, so the discoverability route
  (a tool that lists reserved names) becomes redundant. The reserved
  set is `aether.sink.*`, knowable from the name, not from a list.

## Alternatives considered

**Hard cutover to `aether.<subsystem>` (no `sink` infix).**
Mirrors the existing `aether.control` precedent exactly and saves
five characters per send. Rejected because `aether.camera` would be
string-identical to the existing camera kind name. The two live in
disjoint ID-space domains and don't actually collide at runtime, but
mail logs and `engine_logs` traces would show the same string in two
roles, and a human reading the trace can't tell whether the line
refers to the mailbox or the payload kind. The `sink` infix is worth
its five characters for that reason alone.

**Aliased / soft cutover.** Both `render` and `aether.sink.render`
resolve to the same mailbox during a deprecation window; warn-log
on bare-name use; remove the bare names later. Rejected: the
warn-log isn't enforcement (every fresh Claude still hits it), the
"remove later" milestone tends to slip indefinitely, and there are
no external consumers we can't update in the same PR. Soft cutover
buys nothing and ships permanent dual-resolution code.

**Reserve-list / discovery tool.** Keep the bare names canonical;
add an MCP tool (or a field on `describe_kinds`) that lists reserved
chassis-owned names so the agent can avoid them. Rejected: this
addresses the discoverability symptom while leaving the structural
smell (chassis sinks living in user-space namespace) intact. The
asymmetry with `aether.control` and `hub.claude.broadcast` keeps
pulling. And memory + clean-error already provides discovery — the
issue is precisely that those measures aren't enough; another
measure of the same kind wouldn't be either.

**Do nothing.** The status quo (memory rule + clean error from
PR 263) was tried after issue #265 was first raised on 2026-04-26
and parked. It was reopened on 2026-04-27 because the forcing
function is the agent harness itself: every compact produces a
naive Claude that bumps into the reserved list. Rejected by the
issue author.
