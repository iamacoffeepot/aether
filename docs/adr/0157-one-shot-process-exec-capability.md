# ADR-0157: One-shot process exec capability

- **Status:** Proposed
- **Date:** 2026-07-21

## Context

Running a subprocess to completion and capturing its output is a recurring need in the workspace, and today every consumer hand-rolls it. Three copies of the same exec-and-capture shape exist:

- `crates/aether-anthropic/src/cli.rs` carries the most complete version: it spawns `claude` with piped stdio, writes the prompt to stdin and closes the pipe for EOF, drains stdout on a dedicated thread so a full pipe cannot stall the child, polls `try_wait` on a 10 ms interval against a deadline, and kills plus reaps the child on overrun so no zombie is left behind.
- `xtask/src/transform.rs` forks headless `claude` with a writer thread and a reap-before-surface-error discipline: it waits on the child before joining the stdin writer so an early exit racing a broken pipe never masks the child's real exit cause, guaranteeing the child is reaped on every path.
- `crates/aether-engine/src/server/artifacts.rs` forks `<binary> --describe` with `stdin` nulled to probe a stored binary's manifest, a smaller one-shot capture without the deadline machinery.

Each duplicate re-derives the same hazards — pipe-stall deadlock, zombie children, the write-versus-early-exit race — and each solves them a little differently. None of this is reachable from a wasm component: subprocess execution lives entirely in native chassis code, so a guest actor that wants to shell out to a tool has no mail surface for it. Meanwhile `aether.fs` and `aether.http` already expose their privileged host resources as mail-addressable edge capabilities that any actor, guest or native, can drive. Subprocess execution is the outstanding edge resource without that treatment.

ADR-0078 named `ProcessCapability` as the first chassis-internal actor and sketched an `aether.process.spawn` / `terminate` / `exited` mail surface. That decision addressed process *supervision* — long-lived substrate children that dial back over RPC, register into the engine registry, and stay alive until terminated or evicted. The supervision shape shipped, but it landed inside the fleet/engines capability (the crate is mid-rename from `aether-engine` to `aether-fleet`, #3884), which owns spawn, heartbeat, and teardown of the substrate fleet. The exec-and-capture shape ADR-0078 also gestured at — run a binary to completion, capture its output, hand it back — never shipped. Its placeholder kinds still sit unused in `aether-kinds` (`aether.process.spawn` / `spawn_result` / `terminate` / `terminate_result` / `exited`) and are retired in #3885, freeing the `aether.process` namespace for the capture-shaped surface.

The dispatch primitive this capability needs already exists. ADR-0093's `dispatch_blocking` runs a blocking closure on a substrate worker thread, holds the caller's settlement chain across the off-thread work, and routes the result back into a `#[handler(task)]` completion handler. A multi-second subprocess run is exactly the "reply in a later turn" shape that primitive was built for, and the content-gen caps already drive their provider calls through it.

## Decision

A one-shot process exec capability ships as the `aether-process` crate, owning the `aether.process` mailbox. It exposes subprocess execution as a mail-addressable edge capability alongside `aether.fs` and `aether.http`, so any actor — a wasm component or a native cap — can run a permitted binary to completion and receive its captured output as a typed reply.

### Mail surface

The capability handles one request kind and replies with one result kind:

```text
aether.process.run {
    binary:         String,        // logical name resolved against the allowlist
    args:           Vec<String>,   // argv, passed verbatim — never a shell string
    env:            Vec<EnvVar>,   // explicit child environment entries
    stdin:          Bytes,         // fed to the child's stdin, then EOF
    timeout_millis: u32,           // deadline; the child is killed and reaped on overrun
}

aether.process.run_result ::
    Ok       { exit_code: Option<i32>, stdout: Bytes, stderr: Bytes }
  | TimedOut { stdout: Bytes, stderr: Bytes }
  | Err      { error: ProcessError }

ProcessError ::
    NotPermitted                    // binary absent from the allowlist; refused before any spawn
  | BinaryNotFound                  // the allowlisted path did not resolve to an executable file
  | SpawnFailed  { detail: String } // exec failed for another reason (not executable, permission denied)
  | WaitFailed   { detail: String } // the OS returned an error while waiting on the child
```

`run` is request/reply, and the reply always arrives. The result is an enum rather than a flat struct because the outcomes are enumerable and this repository enumerates them by rule — the neighboring edge caps already do (`FsError`'s typed variants, `FetchResult::Ok | Err`, `AnthropicError`'s variant taxonomy) — and because a wire-visible reply is exactly the schema `describe_kinds` and every caller pin against.

- `Ok` carries a run that reached completion, including a run that exited non-zero. A non-zero exit is a completed run whose result the consumer judges, so it stays `Ok { exit_code: Some(code) }` rather than becoming an error; only the capability's own inability to run or reap the child is an `Err`. This is the one place the general capability's semantics diverge from `aether-anthropic/src/cli.rs`, which folds a non-zero `claude` exit into its provider error because that cap wants success semantics.
- `TimedOut` is its own arm because a deadline overrun carries the partial `stdout` / `stderr` drained before the kill and is a distinct outcome, not an `Ok` wearing a boolean flag.
- `Err` carries a closed `ProcessError` taxonomy. `NotPermitted` is an allowlist refusal that never spawns. `BinaryNotFound` and `SpawnFailed` split the spawn failures the cli.rs loop already distinguishes — `BinaryNotFound` maps its `ErrorKind::NotFound` / `CliNotFound` path (the resolved path is not an executable file), and `SpawnFailed { detail }` carries every other exec failure (not executable, permission denied at exec). `WaitFailed { detail }` maps the cli.rs `try_wait` / `wait` OS-error path. The taxonomy is closed: a future distinction adds an arm through an ADR amendment rather than widening a free-form string.

`stdout` and `stderr` are `Bytes` rather than `String` because a subprocess emits arbitrary bytes; a consumer that wants text decodes lossily at its own boundary, matching how `aether.fs.read` returns bytes.

`exit_code: Option<i32>` on `Ok` is `None` when the child died by signal. Version 1 records only that the exit was signal-borne, and does not carry the signal number; a consumer that needs the specific signal is a follow-up that adds a field, not a v1 promise.

### Dispatch

Each `run` handler dispatches the blocking spawn-and-capture through ADR-0093's `dispatch_blocking`: the closure owns the resolved command, runs the deadline poll / drain / reap loop off the dispatcher on a substrate worker thread, and shapes the `run_result`; a `#[handler(task)]` completion handler re-replies it through the carried `reply_to`. The capability stores no correlation state on the happy path, and the caller's settlement chain stays held across the whole run so `send_mail_traced` observes it as one in-flight unit. The capture loop lifts the battle-tested discipline already proven in `aether-anthropic/src/cli.rs` — dedicated stdout drain thread, 10 ms deadline poll, kill-and-reap on overrun. When a permitted binary forks its own children, the reaper adopts the tunnel's group-reap escalation (`setsid` at fork so a `killpg` takes down the whole process group, SIGTERM then a grace window then SIGKILL) from `crates/aether-mcp/src/bin/aether-tunnel.rs`, so a grandchild holding a pipe open cannot outlive the deadline.

### Security model

The security posture is the reason this capability is a deliberate, reviewed surface rather than a thin `Command` wrapper. Exposing subprocess execution to guest actors is a privilege escalation, so the capability is closed by default and every widening is an explicit configuration act.

- **Deny-by-default binary allowlist.** The capability resolves the request's `binary` field against a configured allowlist of permitted programs; a binary absent from the list is refused before any spawn. The allowlist is empty by default, so a freshly booted capability refuses every request until an operator names the binaries it may run. The list maps a logical name to an absolute path, so the caller never supplies a filesystem path and cannot reach an arbitrary executable.
- **Argv only, no shell, ever.** The request carries `args` as a string array passed straight to `Command`; the capability never interprets a shell string, never spawns `sh -c`, and never performs word-splitting, globbing, or variable expansion. Shell metacharacters in an argument are inert data. This forecloses command injection at construction rather than by sanitizing input.
- **Constructed child environment.** The child's environment is built solely from the request's explicit `env` entries. The capability never inherits the substrate's environment, because that environment holds provider API keys and other fleet secrets that a subprocess must not see. A child receives exactly the variables the caller names and nothing else.
- **Working-directory confinement.** A run's working directory is confined to the `aether.fs` namespace roots (`save`, `assets`, `config`), so a subprocess starts inside the same sandbox the filesystem capability already governs and cannot be pointed at an arbitrary host directory. The confinement reuses the ADR-0041 namespace-root machinery rather than introducing a second notion of "where a run may touch the disk."

### Capability anatomy

The crate follows the capability module anatomy (ADR-0121 / ADR-0122). It owns its `aether.process.*` kinds in `kinds.rs` rather than parking them in `aether-kinds`, since a guest that talks to it depends on the cap crate directly. Identity and runtime split per ADR-0122: a zero-sized `ProcessCapability` marker carries addressability and handled-kind markers so a guest names `ctx.actor::<ProcessCapability>()` without linking native state, and a runtime half owns the allowlist, the resolved working-directory roots, and the in-flight dispatch bookkeeping. Configuration resolves at chassis boot through the ADR-0090 `#[derive(Config)]` layer (argv over env over default) and lands in `init` as the cap's `Config` type; the allowlist and confinement roots are configuration, never a handler-time environment read. A chassis that cannot or will not offer subprocess execution installs the fail-fast unsupported actor that replies `Err` immediately, matching how the headless chassis handles desktop-only caps.

## Consequences

### Positive

- Subprocess execution becomes reachable from wasm components for the first time, through the same mail surface as every other edge resource.
- The three hand-rolled duplicates collapse to one reviewed implementation with one reap discipline, one deadline story, and one security model, so a future consumer mails `aether.process.run` instead of re-deriving pipe-stall and zombie-reap handling.
- The centralized security posture means the deny-by-default allowlist, the no-shell guarantee, and the constructed-environment rule are audited once rather than trusted to hold across scattered call sites.
- The capability composes with settlement and tracing for free by riding ADR-0093 dispatch, so a `run` is observable end-to-end like any other in-flight mail.

### Negative

- A new privileged surface exists that did not before. A misconfigured allowlist that names a dangerous binary, or a future confinement gap, has broader blast radius than a single hand-rolled call site, so the configuration surface warrants careful defaults and review.
- The existing consumers do not migrate for free: `aether-anthropic`, xtask tooling, and the artifacts probe keep their bespoke code until each is deliberately re-pointed at the capability, and the two shapes coexist during that window.
- Working-directory confinement inherits the current lexical, non-canonicalizing containment of the `aether.fs` adapter, which does not defend against symlink escapes; hardening that boundary is shared follow-on work, not a property this ADR can assume.

### Follow-on work

- The implementation lands the `aether-process` crate, wires it into the chassis builders that should offer it, and flips this ADR to Accepted.
- The wasm `aether.anthropic` CLI backend (the ADR-0159 arc) becomes the first migration: its `claude`-subprocess adapter moves from native-only hand-rolled exec onto a guest-reachable `aether.process.run` call.
- xtask-style tooling that forks `claude` or other build helpers is a candidate to re-point once the capability is available on the relevant chassis.

## Alternatives considered

- **Extend the fleet/engines capability instead of a new crate.** Rejected — supervision and exec-and-capture are different shapes. The fleet capability owns long-lived children that register back over RPC and live until terminated or evicted; a one-shot run has no registration, no heartbeat, and no lifetime past its own completion. Folding run-to-completion capture into a supervision cap would blur two distinct lifecycles into one mailbox and one state machine.
- **Keep hand-rolling per consumer.** Rejected — this is the status quo, and it already produced three copies of the deadline / drain / reap loop with subtly different discipline and no shared security model. Every new consumer would re-pay the pipe-stall and zombie-reap tax, and none of it would ever be reachable from a guest actor.
- **Session-shaped interactive processes.** Deferred as a named v2 non-goal. A long-lived process with a live bidirectional stream — write a line, read a line, repeat — is a genuinely different surface with its own backpressure and framing story (the layered stream-actor chain ADR-0078 sketched). This ADR commits only to the one-shot capture shape; the interactive shape waits for its own decision.
