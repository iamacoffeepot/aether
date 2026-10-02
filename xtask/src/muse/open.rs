//! `muse open`: stage a tree, instructions, a brief, and seeded reads, and open
//! a session on them through `muse.session.open`.

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery_kinds::{EncodedArtifact, Ref};
use aether_bloomery_muse::{OpenInput, SessionOpen, offered};
use aether_bloomery_workspace::TreePath;
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::{EngineArgs, SettingsArgs, call, turn_limit};
use crate::bloomery::parse_digest;
use crate::import_commit::Imported;

/// What every session is told ahead of the instructions file.
const PREFACE: &str = "You work only through the offered tree tools, and end your run only by calling `muse-end`: `Done` with a summary once every briefed change is in the tree, `Blocked` with what stopped you when the work cannot be finished, or `Asked` with the one question you cannot go on without. A reply without a tool call does not end the session. A plan's open questions still go in its Questions section; `Asked` is for work that cannot go on without an answer. You cannot build, run, or test anything. The sections below on Commands, the MCP harness, Local checks and CI, and the branch, pull-request, and landing steps of Workflow describe how other agents work; every rule about the code itself applies to you.\n\n";

/// Arguments for `cargo xtask muse open`.
#[derive(Args, Debug)]
pub(super) struct OpenArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The commit whose tracked files the session works on, imported as
    /// `import-commit` does; any revision `git rev-parse` resolves.
    #[arg(long, required_unless_present = "tree", conflicts_with = "tree")]
    commit: Option<String>,
    /// A tree already in the journal, by its digest.
    #[arg(long)]
    tree: Option<String>,
    /// A file holding the first user message.
    #[arg(long)]
    brief: PathBuf,
    /// A file holding the session instructions, sent as the leading developer
    /// message ahead of the brief with [`PREFACE`] chained ahead of it.
    #[arg(long)]
    instructions: PathBuf,
    /// A file naming one tree path per line, each read with `tree.read`
    /// before the first turn; blank lines are skipped.
    #[arg(long)]
    seeds: Option<PathBuf>,
    #[command(flatten)]
    settings: SettingsArgs,
    /// The most turns the session may make before it rests.
    #[arg(long)]
    max_turns: u32,
}

/// Open the session and print `tree=<digest>`, `session=<key>`, and
/// `after=<seq>`, the boundary `wait` reads from.
pub(super) fn run(args: &OpenArgs) -> Result<()> {
    let brief =
        fs::read_to_string(&args.brief).with_context(|| format!("reading the brief {}", args.brief.display()))?;
    let instructions = fs::read_to_string(&args.instructions)
        .map(|text| format!("{PREFACE}{text}"))
        .with_context(|| format!("reading the instructions {}", args.instructions.display()))?;
    let seeds = args.seeds.as_deref().map(seeds).transpose()?.unwrap_or_default();
    let mut engine = args.engine.connect()?;

    let tree = match (&args.commit, &args.tree) {
        (Some(commit), _) => Imported::read(Path::new("."), commit)?.stage(&mut engine)?.root,
        (None, Some(tree)) => parse_digest(tree)?,
        (None, None) => return Err(anyhow!("name the session's tree with --commit or --tree")),
    };

    let (tools, mut artifacts) = offered();
    let input = OpenInput::new(
        args.settings.settings(tools)?,
        Ref::of_text(&instructions),
        Ref::of_text(&brief),
        turn_limit(args.max_turns)?,
        Ref::from_digest(tree),
        seeds,
    );
    let open = EncodedArtifact::new(&input)?;
    let input = Ref::from_digest(open.digest());
    artifacts.extend([EncodedArtifact::text(&instructions), EncodedArtifact::text(&brief), open]);
    engine.stage_artifacts(artifacts)?;

    let session = call::<SessionOpen>(&mut engine, input)?;
    println!("tree={tree}");
    println!("session={session}");
    println!("after={}", session - 1);
    Ok(())
}

/// The tree paths a seeds file names, one per line, blank lines skipped.
fn seeds(path: &Path) -> Result<Vec<TreePath>> {
    fs::read_to_string(path)
        .with_context(|| format!("reading the seeds {}", path.display()))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| TreePath::new(line).map_err(|error| anyhow!("seed {line:?}: {error}")))
        .collect()
}
