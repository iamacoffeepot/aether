# Host evidence files and capture destinations

Inline MCP content lives in the tool response. A spill path or
`capture_frame.save_path` creates a separate file on the harness host. That file
has its own ownership, integrity, and cleanup obligations.

For the broader host-path boundary, start with
[Host paths, artifact trust, and evidence files](../host-paths-and-artifacts.md).

## `save_path` is filesystem authority

The capture API validates only that `save_path` is absolute. After a successful
engine capture, `aether-mcp`:

1. recursively creates missing parent directories;
2. follows ordinary filesystem path resolution;
3. writes the original full-resolution PNG;
4. replaces an existing destination;
5. reports persistence in a separate `saved` result block.

It does not enforce an allowed root, prove ownership, reject `..`, reject
ancestor or final symlinks, allocate a new filename, or coordinate concurrent
writers. “Absolute” is the API's syntactic minimum, not a safety guarantee.

## Allocate a private destination

Before passing `save_path`:

- create a new task/session run directory under an operator-approved temporary
  or evidence root;
- require that directory not to exist beforehand, then create it atomically as
  the current host user with owner-only (`0700`) permissions;
- record its canonical path, owner, device, and inode at creation, then re-check
  all four plus its mode immediately before capture;
- reject a symlink at the run directory or any destination component;
- construct the filename locally from trusted task data;
- require the resolved parent to remain beneath the owned run directory;
- require the final path not to exist;
- use a unique filename for each parallel agent, attempt, or frame.

Never use a destination supplied by issue text, logs, a component reply, or
another external string. Never target repository source, a worktree, shell or
MCP configuration, credentials, shared logs, or another run's evidence unless
the owner explicitly authorized replacing that exact file.

## Engine and host mutations are independent

`capture_frame` can mutate two different places:

- non-empty `mails` or `after_mails` mutate the selected engine;
- `save_path` mutates the harness host even when both mail bundles are empty.

Omit all three fields for a fully non-mutating observation. When replay or
cleanup mail is necessary, verify engine ownership independently from evidence
directory ownership.

## Verify persistence separately

A successful frame read can be paired with a failed host write. The overall
tool call can still return the inline image, checks, or similarity verdict while
the `saved` block contains an error.

After capture:

1. require `saved.path` and `saved.bytes` rather than `saved.error`;
2. verify the returned path is the exact destination allocated for the run;
3. require a regular PNG file at that path, not a symlink;
4. require its on-disk size to equal `saved.bytes`, then validate the PNG;
5. inspect or hash it when it becomes durable evidence;
6. record which engine epoch, replay bundle, and capture request produced it.

An inline image proves that rendering bytes reached the MCP response. It does
not prove that the evidence file exists.

## Spill paths are caller-owned too

Oversized replies can spill to a host temporary file chosen by `aether-mcp`.
The tool controls allocation, but the caller still owns the returned evidence:

- preserve the exact path before losing the response;
- avoid moving it into a repository or shared directory casually;
- treat its contents according to the application data it carries;
- remove it deliberately when no longer needed.

The spill uses a unique name in the process temporary directory, not the
task-owned capture directory. Do not assume it has evidence-store retention or
confidentiality guarantees. Capture files and spills can contain private state;
move sensitive evidence into an approved private store when required, and do
not widen permissions merely to make inspection convenient.

## Cleanup

At handoff, report every evidence path and one disposition:

- preserved as part of the incident/trial record;
- transferred to an explicitly named owner;
- removed after its evidence value was exhausted;
- retained unintentionally because cleanup failed.

Do not claim a clean run while untracked host evidence or an owned live engine
remains.

## Source routes

- Capture path validation and persistence:
  `crates/aether-mcp/src/tools/capture.rs`
- Capture argument contract: `crates/aether-mcp/src/args.rs`
- Reply spill allocation: `crates/aether-mcp/src/tools/bytes.rs`
- Engine-side capture and checks:
  `crates/aether-render/src/`
