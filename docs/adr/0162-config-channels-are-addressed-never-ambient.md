# ADR-0162: Config Channels Are Addressed, Never Ambient

- **Status:** Accepted
- **Date:** 2026-07-22

## Context

Every chassis knob resolves through one source stack — programmatic > argv > env > file > default (ADR-0156 §5) — and every chassis boot runs the unknown-`AETHER_*` sweep (ADR-0090 §4, ADR-0156 §4): a warn-only pass over the process environment that flags any `AETHER_*` key not claimed by a composed config member. The sweep exists because string-keyed ambient config fails silently — a typo'd or stale `export` is indistinguishable from a working one without a check — and its known-key set is composition-derived, so it cannot drift from what actually boots.

The hub broke that symmetry. Hub-spawned substrates are fork+exec children, and children inherit the parent environment, so exporting a knob on the hub became the de-facto way to configure a whole fleet. Three problems follow:

1. **The hub's env speaks to two audiences.** Keys destined for children sit in the hub's environment, and the hub's own composition-derived sweep would flag them as unknown. `with_hub_fleet_passthrough` patches this: a hand-written union of every substrate profile's knobs, documented in ADR-0156 §4 as the aggregate's one deliberate over-approximation. The list is maintained by convention, drifts silently, and the information it encodes — which keys each binary supports — is owned firsthand by the binaries, which are never asked.
2. **Mixed fleets are noisy at the child tier, and no hub-side list can fix it.** Desktop's manifest deliberately excludes the headless tick knob and headless excludes the window/audio/render knobs, so a fleet-wide `AETHER_TICK_HZ` export makes every desktop child warn and a fleet-wide `AETHER_WINDOW_MODE` makes every headless child warn. Ambient inheritance sprays every profile's knobs at every other profile.
3. **Inheritance propagates without limit.** A substrate that forks its own subprocess (the process cap) leaks the same `AETHER_*` environment another generation down, recreating the two-audience problem one level deeper.

Meanwhile the machinery for asking a binary what it supports already exists in outline: the hub's binary store is content-addressed (ADR-0115) and forks `<binary> --describe` once at upload to capture a claim-derived manifest. A binary's config key set and argv surface are pure functions of its bytes — the compose chain is compiled in — so a manifest captured at upload is exact for as long as the hash stands, and a new build is a new hash with a fresh capture. What the manifest lacks today is the config surface itself, and what the `--describe` convention lacks is enforcement: each bin hand-wires the flag handling and manifest assembly (the bloomery chassis assembles its own, because its `build.rs` provenance `env!`s must resolve in its own crate), so conformance rests on authors and agents remembering the pattern.

The spawn path already carries an addressed channel. `spawn_substrate` forwards per-spawn `args`, and argv outranks env in every chassis's stack; the hub already injects `AETHER_RPC_PORT` and `AETHER_BOOT_MANIFEST` per spawn. The derive-emitted argv overlays (`--rpc-port`, `--boot-manifest`, the frame-size flag) mean every value the hub injects has a flag-shaped spelling on the child's own surface.

## Decision

Aether config never crosses a process boundary ambiently. Each channel keeps exactly one audience:

- **argv is the machine channel.** Config addressed to one process by another rides its argv: the hub appends its per-spawn injections (`--rpc-port`, `--boot-manifest`, and the wire frame-size flag when it must pin the cap both ends of the connection share) to the `args` it already forwards, and per-spawn operator config rides `spawn_substrate.args` as it does today. A binary that cannot accept an injected flag fails at spawn — loudly, correlated through the existing `spawn_failed` reporting — instead of silently booting misconfigured. Argv does not propagate to grandchildren, so process isolation holds at every depth without further machinery.
- **env is the human channel.** Environment variables remain for a person at a shell configuring the process they launch directly — which is exactly the audience the typo sweep serves. A hub- or tunnel-forked child never inherits an environment at all: the fork site clears the child environment (`env_clear`) and constructs it from an explicit allowlist of third-party and platform keys copied from the parent — exact names (`PATH`, `HOME`, `TMPDIR`, `USER`, locale, display, TLS and proxy variables, the bloomery mirror's `GITHUB_TOKEN`) plus platform prefix families (`LC_`, `XDG_`, and the Linux display, GPU-driver, and audio-server families) — followed by the fork site's explicit injections. The allowlist is one shared definition, never carries an `AETHER_*` key, and names only surface owned by other software; aether config crosses a process boundary exclusively as argv or as an explicitly injected value on the constructed environment. The process cap already builds user subprocesses this way (`env_clear` plus mail-specified variables, ADR-0157) and stays the strictest form — no allowlist at all.
- **The config file stays the durable operator layer**, unchanged and dormant until needed.

With the hub's environment back to a single audience, `with_hub_fleet_passthrough` is deleted and the hub's known-key set becomes purely composition-derived, like every other chassis. This supersedes the "one documented over-approximation" carve-out of ADR-0156 §4.

The `--describe` contract graduates from convention to enforced machinery, at both ends:

- **Produce side:** a shared chassis-main prelude in `aether-chassis` owns the `--describe` / `--print-config` argv handling and manifest assembly for every chassis binary. Crate-local facts (the `build.rs` provenance `env!`s) are passed in as values, so the bloomery-style constraint is satisfied without forking the flow. A bin routes through the prelude or visibly diverges; conformance is the path of least resistance rather than a pattern to remember.
- **Consume side:** `upload_binary`'s describe-fork validates the manifest shape strictly and rejects a nonconforming binary. Unstorable means unspawnable, so a binary that skipped the contract cannot enter the fleet at all.

The manifest itself is extended to self-report the binary's config surface — its known env keys and argv overlay flags — cached under the content hash and therefore invalidated exactly when the binary changes. The source of truth for "what does this binary support" becomes the binary's own testimony, captured once, never a parallel hand list.

A fleet-wide per-profile catch-all (defaults applied to every spawn of a profile) is explicitly deferred: nothing today demonstrates the need, and per-spawn `args` covers the known cases. If repeated spawns make one worthwhile, the candidates are the existing TOML section vocabulary or spawn-side tooling, designed against the demonstrated need at that point. Likewise deferred: a per-spawn elevate list on `spawn_substrate` (caller names environment keys to copy onto one child's constructed environment) is the natural escape hatch if a third-party key outside the allowlist ever needs to reach one spawn — addressed by construction, added only when a case demonstrates it.

## Consequences

- `with_hub_fleet_passthrough` and its hand-maintained union die; the hub sweeps only what it composes. The drift class ("added a cap knob, forgot the hub list") ceases to exist.
- Child sweeps become high-signal everywhere: the only `AETHER_*` keys in a spawned engine's environment were deliberately addressed to it, so an unknown-key warning means a real mistake again — on every profile in a mixed fleet.
- **Operator surface change:** exporting `AETHER_*` on the hub no longer reaches spawned engines. Fleet-shaped configuration moves to `spawn_substrate.args` (or a future catch-all). Direct launches — a human running a chassis at a shell — keep env exactly as it is.
- The hub's per-spawn injections switch from `command.env` to appended argv. A stored binary lacking the standard overlay flags fails at spawn instead of booting around its assignment; the failure carries the engine id through the existing spawn-failure path.
- Grandchild isolation needs no new mechanism: argv does not inherit, and every supervised fork boundary (hub, tunnel, process cap) constructs its children's environments from nothing.
- The allowlist is the one maintained surface this decision adds. Its failure mode is a missing platform key surfacing as a child subsystem failure (a GPU or audio variable an exotic driver stack needs); the fix is extending the single shared definition. The bounded cost is accepted in exchange for children whose entire environment is accounted for — a stray parent-side driver override or logging variable can no longer shape a child silently.
- Follow-on work, each its own change: the `AETHER_*` scrub + argv injection in `aether-fleet` (and the tunnel's forks); deleting the pass-through; the shared chassis-main prelude; strict manifest validation at upload; the manifest config-surface extension.
- The describe prelude and upload gate close the convention-vs-contract gap this ADR's own machinery would otherwise widen: the manifest's new load-bearing fields are produced by shared code and checked where they are consumed, never by author diligence.

## Alternatives considered

- **Keep inheritance, compute the hub's union dynamically** (from stored binaries' manifests, or a dev-dependency tripwire test pinning the hand list against the spawnable profiles' manifests). Fixes only the hub's copy of the noise; children in a mixed fleet keep warning at each other, and the machinery exists to preserve an ambient channel nothing needs.
- **Keep env as the delivery vehicle but scrub-and-inject addressed keys per spawn.** Halfway: soft-fail semantics hide a misdelivered key (the child warns and defaults instead of dying), and env still propagates to grandchildren, so the scrub must recur at every fork boundary.
- **Introduce a per-profile `[fleet.env]` table in the hub config now.** Builds the new operator surface on the dormant file layer ahead of any demonstrated need; deferred instead.
- **Scrub only the `AETHER_*` prefix and pass everything else through.** The lighter enforcement: a computed denylist over the namespace aether owns, third-party environment untouched. Rejected once the fork sites' real footprint was surveyed — the injected set is five keys across three sites, and the environment children genuinely need is small and enumerable (`PATH`, `HOME`, `TMPDIR`, locale, plus the Linux display/GPU/audio families), so full construction costs one shared allowlist and additionally isolates children from non-aether ambient drift, which a prefix scrub can never see. The process cap had already proven the constructed-environment model in-tree.
