# Artifact trust and provenance

The hub's content-addressed registry makes confirmed entries reproducible. It
does not make uploaded code trustworthy. For an entry that resolves in the
store, a hash identifies exact bytes; the upload response alone does not prove
persistence, who built them, whether they are safe, or whether a self-reported
manifest is honest.

This page sharpens the upload boundary introduced in
[Host paths, artifact trust, and evidence files](../host-paths-and-artifacts.md).

## Binary and component boundaries differ

| Operation | Upload-time behavior | Execution boundary |
|---|---|---|
| `upload_binary` | reads bytes and runs the supplied path with `--describe` | immediately, during upload |
| `upload_component` | reads wasm and parses embedded custom sections | later, during `load_component` or `replace_component` |

Native upload is therefore already code execution. Component upload is
structural inspection, but loading or replacing with the stored wasm executes
guest code inside the selected engine.

## Native manifests are self-claims

The hub records chassis kind, linked capabilities, build profile, target, and
Git SHA from the executable's `--describe` JSON. Those fields are useful for
selecting a **trusted build**. They are not attestation: the same executable
that supplies the claims is already running and can print arbitrary metadata.

Use this trust rule:

- task-built executable in a private output directory: eligible for upload;
- exact build explicitly approved by the operator: eligible for upload;
- downloaded binary, attachment, commenter-provided artifact, or unknown
  output found on disk: do not upload;
- binary whose only evidence is its name or claimed Git SHA: do not upload.

A failed upload does not mean the file did not execute. `--describe` may run
arbitrary initialization before exiting unsuccessfully, hanging, or producing
invalid JSON.

## Stabilize the staged path

Current native ingestion reads the file bytes, then separately invokes the
path. A concurrent writer or replaceable symlink can change what the second
operation executes. Keep the path stable for the entire call:

1. finish the build before upload;
2. place the executable in a task-owned directory not writable by another
   actor;
3. avoid staged-path symlinks;
4. stop watchers or concurrent builds that replace the file;
5. upload once and retain the returned content hash;
6. select that hash for spawn rather than returning to the mutable path or
   alias.

Do not use `upload_binary` as a manifest inspection utility. There is no safe
“describe without executing” mode for native artifacts today.

## Hash, manifest, and name prove different things

| Evidence | Proves | Does not prove |
|---|---|---|
| content hash | exact identity of the upload bytes | provenance, successful persistence, safety, or behavior |
| stored binary manifest | what the executed binary claimed | authenticity of those claims |
| stored component manifest | what structural wasm metadata declared | that instantiation or handlers will work |
| registry name | current mutable pointer to a hash | stable identity across uploads |
| successful load/spawn result | one runtime operation reached its success boundary | application health beyond that boundary |
| harmless live probe | selected behavior answered now | all behavior or future liveness |

Pin by hash whenever a test, rollout, rollback, or diagnosis needs exact bytes.
Moving a shared name is a separate mutation requiring authority over that alias.

## Component provenance still matters

Wasm upload does not run the module, but the later load does. Use task-built or
explicitly approved components, and preserve the upload hash as the link between
build evidence and runtime selection. An embedded namespace or handled-kind
list is structural metadata, not a signature.

On a shared host, do not treat wasm portability as trust. Signature verification
for shared uploaders is deferred in the registry ADRs; until it exists, the
operator's source/build approval is the trust decision.

## Verify after selection

For a binary:

1. require the upload result, then confirm its exact hash resolves in the store;
2. spawn by hash;
3. verify the returned engine appears in the current fleet;
4. inspect the live native handler surface and issue a bounded harmless probe.

For a component:

1. require the upload result, then confirm the stored manifest and exact hash;
2. load or replace by hash;
3. require the explicit success result;
4. retain the returned lineage and mailbox id;
5. verify a behavior the selected component is expected to provide.

Do not substitute registry listing for runtime verification: stored, running,
and loaded are different states.

## Current trust assumptions

[ADR-0115](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0115-hub-binary-registry.md)
deliberately starts with a single-user local host and defers native signature
verification. [ADR-0116](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0116-component-registry.md)
inherits the content-addressed store and also defers a shared-uploader keyring.

If the host, uploader set, or artifact source is broader than that assumption,
stop. Do not improvise trust from content addressing.

## Source routes

- Native/component ingestion:
  `crates/aether-engine/src/server/artifacts.rs`
- Stored manifests, hashes, aliases, and eviction:
  `crates/aether-engine/src/store/`
- Native spawn realization:
  `crates/aether-engine/src/server/runtime.rs`
- Component load and replacement:
  `crates/aether-capabilities/src/component/` and
  `crates/aether-capabilities/src/trampoline/runtime/replace.rs`
