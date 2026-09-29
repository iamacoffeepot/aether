//! Everything a run can be refused for before any container exists.
//!
//! The checks over the run's source come first, in this order, so a refusal
//! they find never contacts the daemon: every input the request cites is
//! stored (`InputMissing`), the tree's `rust-toolchain.toml` asks for what the
//! environment provides (`ToolchainMismatch`), and each step's tool resolves
//! to an executable in the root (`UnknownTool`). Then [`platform`] compares
//! the environment's platform with the daemon's (`PlatformMismatch`).
//!
//! The run tree and each mount are prefetched before the checks, so
//! `rust-toolchain.toml`, the mounts, and the later writes into containers
//! read from the session's map. The environment root is walked on demand, a
//! few reads per tool.

use std::collections::BTreeSet;
use std::io::Read;
use std::str;

use aether_bloomery_kinds::{Name, Node, OpaqueBytes, Ref, Tree};
use aether_data::Storage;

use super::{RunError, Stop, engine_failed, storage_stop};
use crate::runtime::engine::Engine;
use crate::runtime::storage::{SourceReader, StorageSession};
use crate::{Environment, Platform, Provides, Refusal, RunRequest, RustToolchain, ToolName, ToolRecord};

/// The file a tree names its toolchain in, at its root.
const TOOLCHAIN_FILE: &str = "rust-toolchain.toml";

/// The largest `rust-toolchain.toml` read.
const TOOLCHAIN_FILE_MAX_BYTES: u64 = 64 * 1024;

/// What resolution found: the loaded environment and each step's tool, in
/// step order.
pub struct Resolved {
    pub environment: Environment,
    pub tools: Vec<ToolRecord>,
}

/// Load and check everything the run's source holds for `run`.
pub fn resolve(session: &mut StorageSession, run: &RunRequest) -> Result<Resolved, Stop> {
    let mut reader = session.reader();
    let environment: Environment = load(&mut reader, &run.environment, "the environment")?;
    prefetch(&mut reader, &run.tree, "the run tree")?;
    let tree: Tree = load(&mut reader, &run.tree, "the run tree")?;
    for mount in run.mounts.as_slice() {
        prefetch(&mut reader, &mount.tree, "a mount tree")?;
    }
    for stdin in run.steps.as_slice().iter().filter_map(|step| step.stdin.as_ref()) {
        reader
            .require(stdin)
            .map_err(|error| storage_stop(format!("reading the stdin blob {}", stdin.digest()), error))?;
    }

    toolchain(&mut reader, &tree, &environment.provides)?;
    let tools = run
        .steps
        .as_slice()
        .iter()
        .map(|step| tool(&mut reader, &environment, &step.tool))
        .collect::<Result<_, _>>()?;
    Ok(Resolved { environment, tools })
}

/// Compare the environment's platform with the one the daemon runs, read
/// from `GET /info` and mapped to a target triple.
pub fn platform(engine: &Engine, wanted: &Platform) -> Result<(), Stop> {
    let daemon = engine.info().map_err(engine_failed("reading the daemon's platform"))?;
    let architecture = match daemon.architecture.as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };
    let system = match daemon.os.as_str() {
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => {
            return Err(RunError::Shape(format!(
                "the daemon runs an operating system this backend cannot name: {other}"
            ))
            .into());
        }
    };
    let provided = Platform::new(format!("{architecture}-{system}")).map_err(|error| {
        RunError::Shape(format!("the daemon's architecture {architecture:?} is not a target triple segment: {error}"))
    })?;
    if provided == *wanted {
        Ok(())
    } else {
        Err(Stop::refused(Refusal::PlatformMismatch { wanted: wanted.clone(), provided }))
    }
}

/// Load the stored artifact `artifact` names, or refuse the run as
/// `InputMissing` when the source lacks it.
fn load<K: Storage>(reader: &mut SourceReader<'_>, artifact: &Ref<K>, what: &str) -> Result<K, Stop> {
    reader.load(artifact).map_err(|error| storage_stop(format!("loading {what} {}", artifact.digest()), error))
}

/// Read `tree`'s closure into the session, or refuse the run as
/// `InputMissing` naming the first member the source lacks.
fn prefetch(reader: &mut SourceReader<'_>, tree: &Ref<Tree>, what: &str) -> Result<(), Stop> {
    reader.prefetch(tree).map_err(|error| storage_stop(format!("reading {what} {}", tree.digest()), error))
}

/// Check the tree's `rust-toolchain.toml`, when it has one: its channel must
/// be the one the environment provides, and its components and targets a
/// subset of the provided ones.
fn toolchain(reader: &mut SourceReader<'_>, tree: &Tree, provides: &Provides) -> Result<(), Stop> {
    let Some(node) = tree.entries().get(&name_of(TOOLCHAIN_FILE)?) else {
        return Ok(());
    };
    let (Node::File(blob) | Node::Executable(blob)) = node else {
        return Err(RunError::Shape(format!("{TOOLCHAIN_FILE} is not a regular file")).into());
    };
    let wants = parse_toolchain(&read_small(reader, blob)?)?;

    let satisfied = provides.rust.as_ref().is_some_and(|provided| {
        provided.channel() == wants.channel()
            && subset(wants.components(), provided.components())
            && subset(wants.targets(), provided.targets())
    });
    if satisfied {
        Ok(())
    } else {
        Err(Stop::refused(Refusal::ToolchainMismatch {
            tree_wants: wants,
            environment_provides: provides.rust.clone(),
        }))
    }
}

/// Read the whole of a small stored blob, verified against its digest.
fn read_small(reader: &mut SourceReader<'_>, blob: &Ref<OpaqueBytes>) -> Result<Vec<u8>, Stop> {
    let during = format!("reading {TOOLCHAIN_FILE}");
    let reader = reader.open(blob).map_err(|error| storage_stop(format!("opening {TOOLCHAIN_FILE}"), error))?;
    if reader.len() > TOOLCHAIN_FILE_MAX_BYTES {
        return Err(RunError::Shape(format!("{TOOLCHAIN_FILE} is larger than 64 KiB")).into());
    }
    let mut bytes = Vec::new();
    reader
        .take(TOOLCHAIN_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RunError::Read { during, error })?;
    Ok(bytes)
}

/// Parse the `[toolchain]` table's `channel`, `components`, and `targets`.
/// Repeated components or targets count once; any other key is ignored.
fn parse_toolchain(bytes: &[u8]) -> Result<RustToolchain, Stop> {
    let refuse = |why: &str| Stop::from(RunError::Shape(format!("{TOOLCHAIN_FILE} {why}")));
    let text = str::from_utf8(bytes).map_err(|_| refuse("is not UTF-8"))?;
    let table = text.parse::<toml::Table>().map_err(|error| refuse(&format!("does not parse: {error}")))?;
    let toolchain =
        table.get("toolchain").and_then(toml::Value::as_table).ok_or_else(|| refuse("has no [toolchain] table"))?;
    let channel = toolchain.get("channel").and_then(toml::Value::as_str).ok_or_else(|| refuse("names no channel"))?;
    let list = |key: &str| -> Result<Vec<String>, Stop> {
        let Some(value) = toolchain.get(key) else {
            return Ok(Vec::new());
        };
        let items = value.as_array().ok_or_else(|| refuse(&format!("has a {key} that is not a list")))?;
        let unique = items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| refuse(&format!("has a {key} entry that is not a string")))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(unique.into_iter().collect())
    };
    RustToolchain::new(channel, list("components")?, list("targets")?)
        .map_err(|error| refuse(&format!("names a toolchain the kinds refuse: {error}")))
}

/// Whether every item of the sorted `wanted` is in the sorted `provided`.
fn subset(wanted: &[String], provided: &[String]) -> bool {
    wanted.iter().all(|item| provided.binary_search(item).is_ok())
}

/// Resolve `name` through the environment's tool table to the
/// `Node::Executable` its path holds in the root, walking one directory per
/// segment. Anything else there, or no such name, is `UnknownTool`.
fn tool(reader: &mut SourceReader<'_>, environment: &Environment, name: &ToolName) -> Result<ToolRecord, Stop> {
    let unknown = || Stop::refused(Refusal::UnknownTool(name.clone()));
    let tools = environment.tools.as_slice();
    let entry = tools.binary_search_by(|tool| tool.name.cmp(name)).map(|index| &tools[index]).map_err(|_| unknown())?;

    let mut directory: Tree = load(reader, &environment.root, "the environment root")?;
    let mut segments = entry.path.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        let node = directory.entries().get(&name_of(segment)?).cloned();
        match (node, segments.peek()) {
            (Some(Node::Executable(file)), None) => {
                return Ok(ToolRecord { name: name.clone(), path: entry.path.clone(), file });
            }
            (Some(Node::Directory(child)), Some(_)) => directory = load(reader, &child, "an environment directory")?,
            _ => return Err(unknown()),
        }
    }
    Err(unknown())
}

/// A name the caller knows is valid: a constant, or a segment of a
/// `TreePath`, whose every segment is a `Name` by construction.
fn name_of(segment: &str) -> Result<Name, Stop> {
    Name::new(segment).map_err(|error| RunError::Shape(format!("{segment:?} is not a tree name: {error}")).into())
}
