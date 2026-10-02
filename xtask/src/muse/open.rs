//! `muse open`: stage a tree, instructions, a brief, and seeded reads, and open
//! a session on them through `muse.session.open`.

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery_kinds::{EncodedArtifact, Ref};
use aether_bloomery_muse::{OpenInput, SessionOpen, offered, offered_with_proofs};
use aether_bloomery_workspace::TreePath;
use aether_bloomery_workspace_programs::proof::ProofBound;
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::{EngineArgs, SettingsArgs, call, turn_limit};
use crate::bloomery::parse_digest;
use crate::import_commit::Imported;

/// What every session is told ahead of the instructions file.
const PREFACE: &str = "You work only through the offered tools, and end your run only by calling `muse-end`: `Done` with a summary once every briefed change is in the tree, `Blocked` with what stopped you when the work cannot be finished, or `Asked` with the one question you cannot go on without. A reply without a tool call does not end the session. A plan's open questions still go in its Questions section; `Asked` is for work that cannot go on without an answer. Make every independent tool call in the same turn: reads, searches, and edits to different files go together, not one per turn. The sections below on Commands, the MCP harness, Local checks and CI, and the branch, pull-request, and landing steps of Workflow describe how other agents work; every rule about the code itself applies to you.\n\n";

/// What a session offered the proof tools is told after [`PREFACE`].
const PROOFS: &str = "`proof-clippy` formats the whole workspace with `cargo fmt` and checks it with `cargo clippy`, as CI does. Run it once your changes are in, fix what it reports, and run it again until it passes before you end `Done`. It returns the formatted tree, so read a file it rewrote again before you edit it.\n\n";

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
    /// message ahead of the brief with [`PREFACE`] chained ahead of it, and
    /// [`PROOFS`] after that when the session is offered the proof tools.
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
    /// The environment proofs run in, by its digest: the one the head
    /// `(aether.workspace.environment, <platform>)` names. With `--vendor`,
    /// the session is offered the proof tools.
    #[arg(long, requires = "vendor")]
    environment: Option<String>,
    /// The `cargo vendor` tree proofs build against, by its digest: the
    /// `Vendored.tree` of a `vendor.cargo` run over a source with the
    /// session's `Cargo.lock`.
    #[arg(long, requires = "environment")]
    vendor: Option<String>,
}

/// Open the session and print `tree=<digest>`, `session=<key>`, and
/// `after=<seq>`, the boundary `wait` reads from.
pub(super) fn run(args: &OpenArgs) -> Result<()> {
    let ((tools, mut artifacts), preface) = proofs(args.environment.as_deref(), args.vendor.as_deref())?.map_or_else(
        || (offered(), PREFACE.to_owned()),
        |proofs| (offered_with_proofs(&proofs), [PREFACE, PROOFS].concat()),
    );
    let brief =
        fs::read_to_string(&args.brief).with_context(|| format!("reading the brief {}", args.brief.display()))?;
    let instructions = fs::read_to_string(&args.instructions)
        .map(|text| format!("{preface}{text}"))
        .with_context(|| format!("reading the instructions {}", args.instructions.display()))?;
    let seeds = args.seeds.as_deref().map(seeds).transpose()?.unwrap_or_default();
    let mut engine = args.engine.connect()?;

    let tree = match (&args.commit, &args.tree) {
        (Some(commit), _) => Imported::read(Path::new("."), commit)?.stage(&mut engine)?.root,
        (None, Some(tree)) => parse_digest(tree)?,
        (None, None) => return Err(anyhow!("name the session's tree with --commit or --tree")),
    };

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

/// The proofs' environment and vendor tree, when both digests are given.
fn proofs(environment: Option<&str>, vendor: Option<&str>) -> Result<Option<ProofBound>> {
    match (environment, vendor) {
        (Some(environment), Some(vendor)) => Ok(Some(ProofBound::new(
            Ref::from_digest(parse_digest(environment).context("--environment")?),
            Ref::from_digest(parse_digest(vendor).context("--vendor")?),
        ))),
        _ => Ok(None),
    }
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
