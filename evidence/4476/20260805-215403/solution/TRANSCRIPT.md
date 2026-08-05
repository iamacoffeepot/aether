# Aether `resolve_actor` dogfood transcript

## Build

- Build host: Eve (`ssh eve`).
- Source: `/mnt/dev/aether-remote/worktrees/issue-4476-v2`.
- Evidence directory: `/mnt/dev/aether-remote/dogfood/issue-4476/20260805-215403`.
- Cargo target directory: `/mnt/dev/aether-remote/targets/issue-4476-dogfood`.
- Toolchain: `/mnt/dev/aether-remote/rustup-home/toolchains/1.96.0-x86_64-unknown-linux-gnu`.
- Initial invocation failed before compilation because Cargo could not locate `rustc`.
- Retried with `RUSTC` set to the pinned toolchain's `rustc` binary.
- First compilation exposed two authoring corrections: wasm entries reject the `root` placement marker, and the Kind derives required the directly named `aether-data` dependency.
- Corrected wasm build result: success (`Finished dev profile`).
- Artifact: `aether_resolve_actor_dogfood.wasm`, SHA-256 registry hash `d6fadbab3038e2c532e4943e8f8c8507b13ba03dc490efa9c88bb45efdcbcb85`.

## Runtime attempt 1

- Supplied engine: `00000000-0000-0000-0000-000000000031`.
- Upload from the Eve target and evidence paths failed because the hub host could not see either remote path.
- Copying the already-built wasm to this local solution directory allowed upload to succeed.
- Loading the component failed before any actor initialized:

  `wasm instantiation failed: unknown import: aether::spawn_sibling_scoped_p32 has not been defined`

- No setup mail was delivered and no child reply or actor log was available.
- Attempted termination of the supplied engine through `terminate_substrate`; the tool call was rejected by the safety reviewer as a shared-runtime risk. Parent cleanup is required.

## Runtime attempt 2

- The parent terminated the incompatible supplied engine.
- A public `SubstrateHarness` runner was added under `runner/`, built on Eve, and run entirely in process; it spawned or touched no MCP engine.
- The harness loaded the same wasm hash and drove three settlement-gated entry mails: setup, worker spawn, and probe.
- The spawned branch's canonical lineage was `aether.component/aether.embedded:resolve-dogfood/aether.embedded:branch`; the worker's was that lineage plus `/aether.embedded:alpha`.
- The branch log showed, in order:
  1. `spawned worker alpha and deliberately discarded its mailbox id`
  2. `resolved worker alpha by typed key and sent request`
  3. `value=42 observed worker alpha reply`
- The worker log showed `value=41 worker alpha received request`.
- Runner result: `DOGFOOD_SUCCESS entry_mailbox=mbx-apts-lnr3-gfkk reply_value=42`.
- Full successful stdout is preserved in `RUN_TRANSCRIPT.txt`.
