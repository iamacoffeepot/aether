//! `cargo xtask fixtures` — rewrite or check pinned golden wire bytes.
//!
//! The constructors live in `aether_bloomery::testing`; this command is the
//! only writer of the files the golden tests compare against.

#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::testing::{
    containment_refused_event, representative, surface_overlap_decisions, surface_overlap_event,
};
use aether_data::wire::to_vec;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

const FIXTURES_DIR: &str = "crates/aether-bloomery/tests/golden_decisions/fixtures";

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
    /// `containment-refused-event`. Omit the name to rewrite every file.
    Regen {
        /// Fixture name. Omit to rewrite every fixture.
        name: Option<String>,
    },
    /// Report fixtures whose files have drifted from their constructors, without writing.
    Check,
}

struct Fixture {
    name: &'static str,
    file: &'static str,
    test: &'static str,
    encode: fn() -> Result<Vec<u8>>,
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
        encode: encode_decisions,
    },
    Fixture {
        name: "surface-overlap-decisions",
        file: "surface-overlap-decisions.bin",
        test: "surface_overlap_outcome_wire_bytes_match_pinned_golden",
        encode: encode_surface_overlap_decisions,
    },
    Fixture {
        name: "surface-overlap-event",
        file: "surface-overlap-event.bin",
        test: "surface_overlap_event_wire_bytes_match_pinned_golden",
        encode: encode_surface_overlap_event,
    },
    Fixture {
        name: "containment-refused-event",
        file: "containment-refused-event.bin",
        test: "containment_refused_event_wire_bytes_match_pinned_golden",
        encode: encode_containment_refused_event,
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
        fs::write(&path, (fixture.encode)()?).with_context(|| format!("write {}", path.display()))?;
        written.push(fixture.name);
    }
    Ok(written)
}

/// Names whose files under `root` differ from their constructors. Writes nothing.
pub fn check_in(root: &Path) -> Result<Vec<&'static str>> {
    let mut stale = Vec::new();
    for fixture in FIXTURES {
        let encoded = (fixture.encode)()?;
        let on_disk = fs::read(fixture.path(root)).ok();
        if on_disk.as_deref() != Some(encoded.as_slice()) {
            stale.push(fixture.name);
        }
    }
    Ok(stale)
}

/// Append regen commands for any golden fixture tests named in `findings`.
pub fn annotate_findings(findings: &str) -> String {
    let hints: Vec<String> = FIXTURES
        .iter()
        .filter(|fixture| findings.contains(fixture.test))
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
