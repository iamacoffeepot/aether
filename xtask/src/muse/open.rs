//! `muse open`: stage a tree, instructions, a brief, and seeded reads, and open
//! a session on them through `muse.session.open`.

use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery_kinds::{EncodedArtifact, ProgramName};
use aether_bloomery_muse::{
    CacheKey, OpenInput, RequiredProofs, SessionOpen, offered, offered_with_proofs, required_proofs,
};
use aether_bloomery_program::function_name;
use aether_bloomery_workspace::{EnvVar, TreePath};
use aether_bloomery_workspace_programs::proof::{ProofBound, TestEnv};
use aether_data::Ref;
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::activation::muse_activation;
use super::call::call;
use super::{EngineArgs, SettingsArgs, turn_limit};
use crate::bloomery::parse_digest;
use crate::import_commit::Imported;

/// What every session is told ahead of the instructions file.
const PREFACE: &str = "You work only through the offered tools, and end your run only by calling `muse-end`: `Done` with a summary once every briefed change is in the tree, `Blocked` with what stopped you when the work cannot be finished, or `Asked` with the one question you cannot go on without. A reply without a tool call does not end the session. A plan's open questions still go in its Questions section; `Asked` is for work that cannot go on without an answer. Before you end `Done`, run `tree-diff` and read your own changes. Make every independent tool call in the same turn: reads, searches, and edits to different files go together, not one per turn. The sections below on Commands, the MCP harness, Local checks and CI, and the branch, pull-request, and landing steps of Workflow describe how other agents work; every rule about the code itself applies to you.\n\n";

/// What a session offered the proof tools is told after [`PREFACE`].
const PROOFS: &str = "`proof-clippy` formats the whole workspace with `cargo fmt` and checks it with `cargo clippy`, as CI does. `proof-test` formats the whole workspace and runs its tests with the session's test env. Run each once your changes are in, fix what it reports, and run it again until it passes before you end `Done`. Each returns the formatted tree, so read a file it rewrote again before you edit it. `vendor-list`, `vendor-read`, and `vendor-grep` read the vendored crate sources as `tree-list`, `tree-read`, and `tree-grep` read your tree, so read a dependency's API there before you call it.\n\n";

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
    /// [`PROOFS`] after that when the session is offered the proof tools, and
    /// what [`gate_preface`] says after that when its `Done` end must pass
    /// any.
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
    /// A variable the test proof hands cargo, `KEY=VALUE`, repeatable. Needs
    /// `--environment` / `--vendor`.
    #[arg(long, requires = "environment")]
    test_env: Vec<String>,
    /// A proof tool a `Done` end must pass on the session's tree, by program
    /// name (`proof.clippy`, `proof.test`), repeatable; each runs over the
    /// whole workspace. Needs `--environment` / `--vendor`. None gates
    /// nothing.
    #[arg(long, requires = "environment")]
    require: Vec<String>,
    /// The prompt cache key this session shares with every other session
    /// opened with it, drawn fresh when omitted.
    #[arg(long)]
    share_cache_key: Option<String>,
}

/// Open the session and print `tree=<digest>`, `session=<key>`, and
/// `after=<seq>`, the boundary `wait` reads from; refuse when the muse reactor
/// is not live.
pub(super) fn run(args: &OpenArgs) -> Result<()> {
    let proofs = proofs(args.environment.as_deref(), args.vendor.as_deref(), &args.test_env)?;
    let preface = proofs.as_ref().map_or_else(|| PREFACE.to_owned(), |_| [PREFACE, PROOFS].concat());
    let (required, required_args) = required(proofs.as_ref(), &args.require)?;
    let preface = format!("{preface}{}", gate_preface(&required)?);
    let brief =
        fs::read_to_string(&args.brief).with_context(|| format!("reading the brief {}", args.brief.display()))?;
    let instructions = fs::read_to_string(&args.instructions)
        .map(|text| format!("{preface}{text}"))
        .with_context(|| format!("reading the instructions {}", args.instructions.display()))?;
    let seeds = args.seeds.as_deref().map(seeds).transpose()?.unwrap_or_default();
    let mut engine = args.engine.connect()?;
    muse_activation(&mut engine)?.refuse_unless_live(&args.engine.unit)?;

    let tree = match (&args.commit, &args.tree) {
        (Some(commit), _) => Imported::read(Path::new("."), commit)?.stage(&mut engine)?.root,
        (None, Some(tree)) => parse_digest(tree)?,
        (None, None) => return Err(anyhow!("name the session's tree with --commit or --tree")),
    };
    let (tools, mut artifacts) = proofs
        .as_ref()
        .map_or_else(|| offered(Ref::from_digest(tree)), |proofs| offered_with_proofs(Ref::from_digest(tree), proofs));
    artifacts.extend(required_args);

    let mut input = OpenInput::new(
        args.settings.settings(tools)?,
        Ref::of_text(&instructions),
        Ref::of_text(&brief),
        turn_limit(args.max_turns)?,
        Ref::from_digest(tree),
        seeds,
        required,
    );
    if let Some(key) = shared_key(args.share_cache_key.as_deref())? {
        input = input.sharing(key);
    }
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

/// The session's shared prompt cache key, when `--share-cache-key` names one.
pub(super) fn shared_key(value: Option<&str>) -> Result<Option<CacheKey>> {
    value.map(|key| CacheKey::new(key).map_err(|error| anyhow!("--share-cache-key {key:?}: {error}"))).transpose()
}

/// The proofs' environment, vendor tree, and test env, when the environment and vendor digests are given.
fn proofs(environment: Option<&str>, vendor: Option<&str>, test_env: &[String]) -> Result<Option<ProofBound>> {
    match (environment, vendor) {
        (Some(environment), Some(vendor)) => Ok(Some(ProofBound::new(
            Ref::from_digest(parse_digest(environment).context("--environment")?),
            Ref::from_digest(parse_digest(vendor).context("--vendor")?),
            parse_test_env(test_env)?,
        ))),
        _ => Ok(None),
    }
}

/// The proofs each `--require` value names among those offered over
/// `proofs`, with the arguments to stage; none when no proof is offered,
/// which `--require` needing `--environment` leaves only with no value.
fn required(proofs: Option<&ProofBound>, names: &[String]) -> Result<(RequiredProofs, Vec<EncodedArtifact>)> {
    let Some(proofs) = proofs else {
        return Ok((RequiredProofs::default(), Vec::new()));
    };
    let names = names
        .iter()
        .map(|name| ProgramName::new(name.as_str()).map_err(|error| anyhow!("--require {name:?}: {error}")))
        .collect::<Result<Vec<_>>>()?;
    required_proofs(proofs, &names).map_err(|error| anyhow!("--require: {error}"))
}

/// What a session whose `Done` end must pass `required` is told after
/// [`PROOFS`], naming each proof by its function name, or nothing when no
/// proof is required.
fn gate_preface(required: &RequiredProofs) -> Result<String> {
    let proofs = required
        .as_slice()
        .iter()
        .map(|proof| function_name(proof.program()).map(|function| format!("`{function}`")))
        .collect::<Result<Vec<_>, _>>()?;
    if proofs.is_empty() {
        return Ok(String::new());
    }
    let proofs = proofs.join(" and ");
    Ok(format!(
        "Ending `Done` runs {proofs} on your tree, and the run ends only once each passes; a failure answers your \
         `muse-end` call with what failed, and the session goes on.\n\n"
    ))
}

/// The session-supplied test env: each `--test-env` value split at its first
/// `=`.
pub(super) fn parse_test_env(values: &[impl AsRef<str>]) -> Result<TestEnv> {
    let vars = values
        .iter()
        .map(|value| {
            let value = value.as_ref();
            let (key, var) = value.split_once('=').ok_or_else(|| anyhow!("--test-env {value:?} is not KEY=VALUE"))?;
            EnvVar::new(key, var).map_err(|error| anyhow!("--test-env {value:?}: {error}"))
        })
        .collect::<Result<Vec<EnvVar>>>()?;
    let keys: Vec<String> = vars.iter().map(|var| var.key().to_owned()).collect();
    TestEnv::new(vars).map_err(|error| anyhow!("--test-env {keys:?}: {error}"))
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
