//! `cargo xtask fixtures` — rewrite or check pinned golden wire bytes.
//!
//! The constructors live in `aether_bloomery::testing`; this command is the
//! only writer of the files the golden tests compare against.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::persisted::PERSISTED_KINDS;
use aether_bloomery::testing::{
    containment_refused_event, representative, surface_overlap_decisions, surface_overlap_event,
};
use aether_data::wire::to_vec;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

const FIXTURES_DIR: &str = "crates/aether-bloomery/tests/golden_decisions/fixtures";
const SCHEMA_DIGESTS_FILE: &str = "schema-digests.txt";
const SCHEMA_DIGESTS_TEST: &str = "pinned_schema_digests_match_the_registry";
const SCHEMA_DIGESTS_HINT: &str = "append the new digest to `schema-digests.txt` and register an upcast";

/// `cargo xtask fixtures`.
#[derive(Args, Debug)]
pub struct FixturesArgs {
    #[command(subcommand)]
    command: FixturesCommand,
}

#[derive(Subcommand, Debug)]
enum FixturesCommand {
    /// Rewrite pinned fixture files from their constructors.
    ///
    /// Names: `decisions`, `surface-overlap-decisions`, `surface-overlap-event`,
    /// `containment-refused-event`, `schema-digests`. Omit the name to rewrite
    /// every file. Byte-fixtures overwrite; `schema-digests` only appends.
    Regen {
        /// Fixture name. Omit to rewrite every fixture.
        name: Option<String>,
    },
    /// Report fixtures whose files have drifted from their constructors, without writing.
    Check,
}

/// How regen writes this fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteMode {
    /// Replace the file with the constructor's bytes.
    Overwrite,
    /// Append newly current schema digests; never drop a prior line.
    Append,
}

struct Fixture {
    name: &'static str,
    file: &'static str,
    test: &'static str,
    mode: WriteMode,
    encode: Option<fn() -> Result<Vec<u8>>>,
}

impl Fixture {
    fn path(&self, root: &Path) -> PathBuf {
        root.join(FIXTURES_DIR).join(self.file)
    }
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "decisions",
        file: "decisions.bin",
        test: "decisions_wire_bytes_match_pinned_golden",
        mode: WriteMode::Overwrite,
        encode: Some(encode_decisions),
    },
    Fixture {
        name: "surface-overlap-decisions",
        file: "surface-overlap-decisions.bin",
        test: "surface_overlap_outcome_wire_bytes_match_pinned_golden",
        mode: WriteMode::Overwrite,
        encode: Some(encode_surface_overlap_decisions),
    },
    Fixture {
        name: "surface-overlap-event",
        file: "surface-overlap-event.bin",
        test: "surface_overlap_event_wire_bytes_match_pinned_golden",
        mode: WriteMode::Overwrite,
        encode: Some(encode_surface_overlap_event),
    },
    Fixture {
        name: "containment-refused-event",
        file: "containment-refused-event.bin",
        test: "containment_refused_event_wire_bytes_match_pinned_golden",
        mode: WriteMode::Overwrite,
        encode: Some(encode_containment_refused_event),
    },
    Fixture {
        name: "schema-digests",
        file: SCHEMA_DIGESTS_FILE,
        test: SCHEMA_DIGESTS_TEST,
        mode: WriteMode::Append,
        encode: None,
    },
];

fn encode_decisions() -> Result<Vec<u8>> {
    encode(&representative(), "decisions")
}

fn encode_surface_overlap_decisions() -> Result<Vec<u8>> {
    encode(&surface_overlap_decisions(), "surface-overlap-decisions")
}

fn encode_surface_overlap_event() -> Result<Vec<u8>> {
    encode(&surface_overlap_event(), "surface-overlap-event")
}

fn encode_containment_refused_event() -> Result<Vec<u8>> {
    encode(&containment_refused_event(), "containment-refused-event")
}

fn encode(value: &impl Serialize, name: &str) -> Result<Vec<u8>> {
    to_vec(value).with_context(|| format!("encode {name}"))
}

/// Dispatch `cargo xtask fixtures`.
///
/// # Errors
/// Encoding fails, a named fixture is unknown, or a write fails.
pub fn run(args: &FixturesArgs) -> Result<()> {
    match &args.command {
        FixturesCommand::Regen { name } => {
            for fixture in regen_in(&workspace_root(), name.as_deref())? {
                println!("wrote {fixture}");
            }
            Ok(())
        }
        FixturesCommand::Check => {
            let stale = check_in(&workspace_root())?;
            if stale.is_empty() {
                return Ok(());
            }
            bail!("stale fixtures: {}", stale.join(", "));
        }
    }
}

/// Rewrite selected fixtures under `root`. Returns the names written.
pub fn regen_in(root: &Path, name: Option<&str>) -> Result<Vec<&'static str>> {
    let selected = select(name)?;
    let mut written = Vec::new();
    for fixture in selected {
        let path = fixture.path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        match fixture.mode {
            WriteMode::Overwrite => {
                let encode = fixture.encode.context("overwrite fixtures encode")?;
                fs::write(&path, encode()?).with_context(|| format!("write {}", path.display()))?;
            }
            WriteMode::Append => {
                let existing = fs::read_to_string(&path).ok();
                fs::write(&path, schema_digest_file(existing.as_deref()))
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        written.push(fixture.name);
    }
    Ok(written)
}

/// Names whose files under `root` differ from their constructors. Writes nothing.
pub fn check_in(root: &Path) -> Result<Vec<&'static str>> {
    let mut stale = Vec::new();
    for fixture in FIXTURES {
        match fixture.mode {
            WriteMode::Overwrite => {
                let Some(encode) = fixture.encode else {
                    continue;
                };
                let encoded = encode()?;
                let on_disk = fs::read(fixture.path(root)).ok();
                if on_disk.as_deref() != Some(encoded.as_slice()) {
                    stale.push(fixture.name);
                }
            }
            WriteMode::Append => {
                if schema_digests_are_stale(&fixture.path(root)) {
                    stale.push(fixture.name);
                }
            }
        }
    }
    Ok(stale)
}

/// Append a regen command for a byte-fixture failure, or the append-and-upcast
/// hint for the schema-digest gate. Never names a regen command for that gate.
pub fn annotate_findings(findings: &str) -> String {
    if findings.contains(SCHEMA_DIGESTS_TEST) {
        return format!("{findings}\n{SCHEMA_DIGESTS_HINT}");
    }
    let hints: Vec<String> = FIXTURES
        .iter()
        .filter(|fixture| fixture.mode == WriteMode::Overwrite && findings.contains(fixture.test))
        .map(|fixture| {
            let name = fixture.name;
            format!("run `cargo xtask fixtures regen {name}`")
        })
        .collect();
    if hints.is_empty() {
        findings.to_owned()
    } else {
        format!("{findings}\n{}", hints.join("\n"))
    }
}

fn schema_digests_are_stale(path: &Path) -> bool {
    let on_disk = fs::read_to_string(path).unwrap_or_default();
    let last_by_kind = last_pinned_by_kind(&on_disk);
    PERSISTED_KINDS.iter().any(|kind| last_by_kind.get(kind.name).copied() != Some(kind.current_digest()))
}

fn last_pinned_by_kind(text: &str) -> BTreeMap<&str, aether_bloomery::Digest> {
    let mut last = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else {
            continue;
        };
        let Some(hex) = parts.next() else {
            continue;
        };
        if let Some(digest) = aether_bloomery::Digest::from_hex(hex) {
            last.insert(kind, digest);
        }
    }
    last
}

fn schema_digest_file(existing: Option<&str>) -> String {
    let mut lines: Vec<String> =
        existing.unwrap_or("").lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned).collect();
    let last = last_pinned_by_kind(existing.unwrap_or(""));
    for kind in PERSISTED_KINDS {
        let current = kind.current_digest();
        if last.get(kind.name).copied() == Some(current) {
            continue;
        }
        lines.push(format!("{} {current}", kind.name));
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut body = lines.join("\n");
        body.push('\n');
        body
    }
}

fn select(name: Option<&str>) -> Result<Vec<&'static Fixture>> {
    name.map_or_else(
        || Ok(FIXTURES.iter().collect()),
        |name| {
            FIXTURES.iter().find(|fixture| fixture.name == name).map(|fixture| vec![fixture]).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown fixture {name:?}; known: {}",
                    FIXTURES.iter().map(|fixture| fixture.name).collect::<Vec<_>>().join(", ")
                )
            })
        },
    )
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}
