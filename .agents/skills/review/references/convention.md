# Convention and Architecture Lens

Require a citation to `AGENTS.md`, an accepted ADR, `docs/guide/`, or an established public identifier. Verify the source; do not cite remembered convention.

## Categories

- `units`: abbreviates units that repository rules require to be spelled out.
- `type-in-name`: encodes a Rust primitive type in an identifier.
- `generics`: uses a multi-letter generic name that reads like a type alias.
- `terminology`: invents a synonym for a concept already named by authoritative repository sources.
- `driver-naming`: calls an active driver a passive capability.
- `module-siblings`: adds suffix siblings where a parent module directory is required.
- `actor-state`: adds locks, cells, or atomics to actor state contrary to the run-token architecture.
- `adr-conformance`: violates mail-only actor communication, substrate/actor boundaries, lineage addressing, or another accepted decision.

## Bar

Include the exact rule path and rule text or term in the rationale. A new concept with no established name is not a terminology violation. Route an already-gated rule to `lintCandidates` with `gate-gap`; do not present clippy, formatting, Qodana, or repository check failures as judgment findings.
