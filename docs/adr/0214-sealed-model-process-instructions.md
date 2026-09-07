# ADR-0214: Sealed model-process instructions

- **Status:** Accepted
- **Date:** 2026-09-07

## Context

Bloomery distinguishes the source being changed from the process responsible for changing and judging it.

Today, model-process instructions—including construct, review, scope, and shared lane guidance—are embedded in `xtask` through `include_str!`. `ProcessTransformRunner` runs the lane program inside the dispatched checkout. Consequently, changing the candidate can also change the instructions or assembly code used by a subsequent lane.

That makes the process itself part of the candidate under examination. A receipt cannot reliably describe which process judged a candidate when the candidate can replace that process.

ADR-0149 requires `PromptManifest` instruction slots to trace to signed statements or versioned policy artifacts. ADR-0174 supplies `ConfigRegistry` for content-addressed configuration. Neither hashing candidate files nor storing them as artifacts establishes that they are authorized policy.

This decision specifies where model-process instructions obtain their authority and how an execution keeps that choice fixed.

## Decision

### Model-process instructions are explicit configuration

Introduce a content-addressed instruction-bundle configuration using ADR-0174’s existing `ConfigRegistry`, rather than adding another field to `BloomSpec`.

The bundle identifies the instruction bytes for each model process, including shared conventions and authoritative prompt framing.

The host operator authorizes which bundles may serve as process policy. Uploading a bundle, knowing its digest, or naming it in a request does not by itself authorize it.

This configuration is bloom-wide. A member cannot substitute its own process-policy bundle.

### Resolve defaults before sealing

Before sealing a bloom, the host resolves the selected instruction bundle, verifies its authorization and completeness, and records its digest in the bloom’s configuration.

A default may simplify authoring, but it must become an explicit pin before sealing. Dispatch must not consult whatever default happens to be installed later.

Changing the bundle creates a different sealed configuration. An existing bloom does not adopt new instructions midway through execution.

Pre-bloom scope runs follow the same principle: pin the bundle when creating the durable scope run and retain that pin across retries.

### The candidate cannot replace the process

Model invocation and authoritative prompt assembly run through a host-managed execution path, outside candidate control.

The candidate checkout remains the material being worked on. Its copies of instruction files or prompt-assembly code do not select or replace the process for that execution.

Automatic repository-instruction discovery must not silently promote candidate-controlled files into process instructions. Files read from the candidate may supply task material or context, but do not acquire policy authority merely because of their filenames.

This applies to local and Actions execution. Passing pinned text to a candidate-controlled launcher is not sufficient enforcement.

### Validate and record what is dispatched

Before invoking a model, the host must:

- Resolve the exact pinned bundle and verify its content.
- Assemble and validate the prompt manifest using `assemble_manifest`.
- Persist the manifest and its instruction-bundle identity as attempt evidence.
- Ensure the invocation consumes the validated inputs, without substituting candidate or ambient instructions.

Missing content, digest mismatches, unauthorized bundles, or incompatible execution paths refuse dispatch with an explicit reason. They do not fall back to instructions from the checkout.

Retries and resumed attempts preserve the same process pin. A changed process must not reuse a conversation or receipt as though nothing changed.

### Process policy does not authorize arbitrary task text

An authorized instruction bundle establishes the process rules, not the authority of every task submitted to that process.

Work orders and derived instructions retain ADR-0149’s provenance requirements. Recorded derivation from grounded intent can authorize generated work without requiring a new signature on every generated artifact.

ADR-0182’s door separation remains unchanged: an Approve or Answer signature does not become a Ground signature. Nor may arbitrary task text be granted authority simply by attaching the instruction bundle as a parent.

## Migration

Historical journals and receipts remain readable and unchanged. The migration must not invent instruction pins for past executions or rewrite sealed bloom identities.

After enforcement is enabled, an old bloom lacking the required pin cannot start another model attempt. Continuing it requires a successor with explicit configuration. An unpinned scope run similarly requires a replacement run.

Existing instruction files may seed the first bundle through an explicit operator-authorized import. Candidate edits are not automatically imported.

Acceptance of this ADR does not authorize deployment or interruption of a live fleet.

## Consequences

- Editing a candidate no longer changes the process used to judge it.
- Process updates remain possible, but their adoption is explicit and recorded.
- Reproducing an attempt can recover its instruction bundle and manifest.
- Implementation requires a host-managed invocation boundary, transport support, retained bundle artifacts, and an operational migration.
- This closes one part of #5589. Production provenance admission and derivation records still need implementation.
- This establishes provenance of the supplied instructions; it does not guarantee model obedience or eliminate prompt injection from source and context.

## Alternatives considered

- **Use instruction files from the candidate:** retains the self-modifying evaluation process.
- **Hash candidate instructions at dispatch:** records their identity without establishing their authority.
- **Read instructions from the sealed base:** a viable alternative, but couples process selection to application source history. Explicit configuration permits independent, auditable process updates.
- **Use the coordinator’s current embedded instructions:** prevents candidate substitution but allows upgrades to change an in-flight bloom’s process.
- **Pin only the prompt text:** insufficient while candidate-controlled assembly or automatic instruction discovery can replace or augment it.
