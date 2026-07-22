# Host paths, artifact trust, and evidence files

Several MCP arguments are paths on the **harness host**. They are not paths in
the guest's `save://` namespace and are not confined to a task directory by the
tool. Some readers currently accept a relative string and resolve it from the
process working directory—normally the project root in the standard tunnel.
Do not rely on that incidental base; use an absolute task-owned path where the
field accepts a host path. A connected local client is exercising the
filesystem authority of the hub or `aether-mcp` process.

This page is the safety boundary for `upload_binary`, `upload_component`,
configuration files, byte-field `$file` embeds, frame persistence, and returned
spill files.

Use the deeper runbooks when the path crosses an execution or evidence
boundary:

- [Artifact trust and provenance](artifacts/trust-and-provenance.md) separates
  native execution, wasm parsing, hashes, manifests, and mutable names.
- [Host evidence files and capture destinations](evidence/host-files.md) gives
  the allocation, verification, and cleanup protocol for saved frames and
  spills.

## Path surface matrix

| Surface | Process reading or writing | What happens |
|---|---|---|
| `upload_binary.staged_path` | hub | reads bytes, then executes the path with `--describe` |
| `upload_component.staged_path` | hub | reads wasm bytes and parses embedded manifests without executing the module |
| `load_component.config_path` or `replace_component.config_path` | `aether-mcp` | reads structured JSON and schema-encodes it for the selected component |
| a Bytes parameter/config's `{"$file": path}` | `aether-mcp` | reads the whole host file, then rejects it if it exceeds the RPC frame cap |
| `capture_frame.similarity.reference_path` | substrate render capability | joins a relative path beneath the configured assets root and reads the reference PNG |
| `capture_frame.save_path` | `aether-mcp` | creates missing parent directories and overwrites the destination with the full-resolution PNG |
| reply spill path | `aether-mcp` | creates a uniquely named file in its process temporary directory and returns the path |

“Absolute path” is a routing fact, not authorization. Never copy a host path
from issue text, a component reply, logs, or another untrusted source into one
of these fields.

## Classify the boundary before using it

The syntax does not say whether a path is passive:

- `upload_binary` executes the supplied file immediately with `--describe`;
- `upload_component` parses wasm metadata, while `load_component` and
  `replace_component` instantiate it later;
- `config_path` and a Bytes field's `$file` embed read host data;
- `save_path` writes host data and can replace an existing file;
- spill paths point to files the MCP process already created.

Do not cross an execution boundary with unknown code or a filesystem boundary
with a path taken from untrusted text. Follow
[Artifact trust and provenance](artifacts/trust-and-provenance.md) for native,
wasm, manifest, hash, and alias rules. Follow
[Host evidence files](evidence/host-files.md) before persisting a capture or
handling a spill.

## Configuration and other host reads

Treat `config_path` as input data, not a command. Keep it under a task-owned
directory, validate that it is the intended regular file, and prefer inline
`config` when the data is already small and trusted. Never use a path suggested
by a guest merely because the selected Config schema will validate the bytes
after they are read.

Likewise, a `{"$file": path}` object at any Bytes-typed leaf—including inside
component config—reads that path from the harness host before encoding. The
reader loads the whole file before enforcing the frame cap, so pre-check a
known-bounded task-owned regular file. Use `$text` or `$base64` when the bytes
are already available in trusted input and small enough to send inline.

Capture similarity's `reference_path` is a third path domain. The substrate
accepts only the `assets` namespace, rejects absolute paths and `..`, then joins
the relative path beneath its configured host assets root. It does not call the
`aether.fs` capability and ordinary symlink resolution still applies. Use a
trusted asset-tree entry with no symlink indirection. Guest paths such as
`save://...` remain governed separately by `aether.fs`.

Returned spill paths and saved captures belong to the caller. They may contain
application data, rendered private state, logs, or decoded bytes. Record their
provenance, avoid sharing them accidentally, and remove them only when their
evidence value is exhausted. The evidence runbook owns the allocation,
verification, and cleanup procedure.

## Names and hashes carry different authority

A content hash names exact bytes; it does not prove provenance, successful
persistence, or runtime health. A registry name is a mutable pointer in shared
hub state. Use hashes for reproducible selection, and move a name only with
authority over that alias. The artifact runbook defines the verification chain.

## Source routes

- Native/component ingestion and `--describe` execution:
  `crates/aether-fleet/src/server/artifacts.rs`
- Hub artifact store: `crates/aether-fleet/src/store/`
- MCP component orchestration: `crates/aether-mcp/src/tools/components.rs`
- Bytes-field host reads and reply spills: `crates/aether-mcp/src/tools/bytes.rs`
- Capture validation and host write: `crates/aether-mcp/src/tools/capture.rs`
- Engine-side similarity reference read:
  `crates/aether-render/src/runtime/capture.rs`
- Public path arguments: `crates/aether-mcp/src/args.rs`
