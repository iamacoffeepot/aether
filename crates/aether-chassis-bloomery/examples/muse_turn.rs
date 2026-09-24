//! Owner-run live smoke: one `muse.turn` on the shipped bloomery composition, authenticated by an engine secret.
//!
//! The runner seeds a fresh journal at a path you name with the `muse` bundle, the conversation texts, and a
//! `muse.turn.input`, boots the bloomery chassis in process over it with the `aether-bloomery` flags you pass after
//! `--`, and makes one `muse.turn` call. It prints the call outcome, the HTTP status, the outcome variant, the answer
//! text, the four usage counts, the raw body's digest, and the journal path. It never prints a request header, the
//! raw body, or a flag value, and the journal is kept for inspection.
//!
//! A live run spends vendor tokens, so only the owner runs it against a vendor, by hand. It is never a test and CI
//! never runs it (ADR-0234 decision 8).
//!
//! The program sends no credential of its own. The key comes from the engine secrets mechanism (ADR-0235): the
//! `--http-secrets <host>/bearer=<name>` binding makes `aether.http` attach `Authorization: Bearer <value>` to a
//! request for exactly that host, over HTTPS only. A plain-http endpoint on a bound host is refused before anything
//! is dialed. The value never enters the program, its input, its result, the journal, or any mail.
//!
//! Procedure:
//!
//! 1. Create the secrets directory `0700` and the `muse` secret file `0600` holding the key (see the
//!    `supplying-secrets` guide recipe).
//! 2. Pre-flight the flags with `aether-bloomery --print-config --secrets-dir <dir> --http-allowlist <vendor-host>
//!    --http-secrets <vendor-host>/bearer=muse`: its `SECRETS` section lists `muse` as `set`.
//! 3. `cargo xtask build-wasm`, so the `muse` bundle is built.
//! 4. Run one turn:
//!
//!    ```text
//!    cargo run -p aether-chassis-bloomery --example muse_turn -- \
//!      --journal <absolute path that does not exist yet> \
//!      --endpoint https://<vendor-host>/<responses path> \
//!      --model <model> --prompt "Say hello." --max-output-tokens 64 \
//!      -- --secrets-dir <dir> --http-allowlist <vendor-host> --http-secrets <vendor-host>/bearer=muse
//!    ```
//!
//!    Expect status 200 and `Completed`. A 401 or 403 is recorded as `Rejected`: check the binding.
//! 5. Confirm the key never reached the journal: `grep -c -F -f <dir>/muse <journal>*` prints 0 for every file.
//!
//! Without spend, `--endpoint http://127.0.0.1:9/v1/responses -- --http-allowlist 127.0.0.1` records a refused turn
//! (nothing listens there) and exits cleanly.

use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::iter;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aether_bloomery_journal::{Batch, Journal};
use aether_bloomery_kinds::{
    Call, CallOutcome, Digest, Head, NativeOrigin, OpaqueBytes, ProgramName, RecordedHead, RecordedHeadMove, Ref,
    Utf8Text,
};
use aether_bloomery_muse::{
    Endpoint, ModelName, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems, TurnOutcome, TurnResult,
    TurnUsage,
};
use aether_chassis_bloomery::BloomeryCli;
use aether_harness_bloomery::SeededJournal;
use aether_harness_substrate::test_helpers::require_wasm;
use clap::{Parser, ValueEnum};

/// The head the seed binds to the `muse` bundle.
const MUSE: Head<OpaqueBytes> = Head::new("muse");

/// How long the runner waits for the turn's outcome: four minutes, past the turn's own 180-second HTTP timeout, so a
/// slow live reply is recorded and read back rather than abandoned mid-flight.
const PATIENCE: Duration = Duration::from_mins(4);

/// The `aether-bloomery` flags that select sources or exit before boot; the runner boots in process and honors none.
const UNHONORED: [&str; 3] = ["--config", "--print-config", "--describe"];

/// One live `muse.turn`, recorded in a journal the runner keeps.
#[derive(Parser)]
#[command(name = "muse_turn")]
struct Args {
    /// Absolute path of the journal file to create. It must not exist yet, and it is kept after the run.
    #[arg(long)]
    journal: PathBuf,
    /// The responses-API URL the turn posts to.
    #[arg(long)]
    endpoint: String,
    /// The model that answers.
    #[arg(long)]
    model: String,
    /// The user message.
    #[arg(long)]
    prompt: String,
    /// An optional developer message sent ahead of the prompt.
    #[arg(long)]
    developer: Option<String>,
    /// The most output tokens the turn may produce.
    #[arg(long, default_value_t = 256)]
    max_output_tokens: u32,
    /// How much reasoning the model spends before it answers.
    #[arg(long, value_enum, default_value_t = Reasoning::Low)]
    reasoning: Reasoning,
    /// `aether-bloomery` flags, after `--`, passed through verbatim.
    #[arg(last = true)]
    chassis: Vec<String>,
}

/// The `--reasoning` values.
#[derive(Clone, Copy, ValueEnum)]
enum Reasoning {
    Low,
    Medium,
    High,
}

impl From<Reasoning> for ReasoningEffort {
    fn from(reasoning: Reasoning) -> Self {
        match reasoning {
            Reasoning::Low => Self::Low,
            Reasoning::Medium => Self::Medium,
            Reasoning::High => Self::High,
        }
    }
}

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let args = Args::parse();
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();

    if !args.journal.is_absolute() || args.journal.exists() {
        writeln!(err, "--journal must be an absolute path that does not exist yet: {}", args.journal.display())?;
        return Ok(ExitCode::FAILURE);
    }
    if let Some(flag) = args.chassis.iter().find(|flag| UNHONORED.iter().any(|unhonored| flag.starts_with(unhonored))) {
        writeln!(err, "{flag} is not honored by this runner; run `aether-bloomery` for it")?;
        return Ok(ExitCode::FAILURE);
    }
    let cli = match BloomeryCli::try_parse_from(iter::once("aether-bloomery".to_owned()).chain(args.chassis.clone())) {
        Ok(cli) => cli,
        Err(error) => {
            writeln!(
                err,
                "the aether-bloomery flags after `--` did not parse ({}); see `aether-bloomery --help`",
                error.kind()
            )?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let Some(wasm) = require_wasm("aether_bloomery_muse") else {
        writeln!(err, "the muse bundle is not built: run `cargo xtask build-wasm`")?;
        return Ok(ExitCode::FAILURE);
    };

    let (batch, input) = seed(&args, &fs::read(wasm)?)?;
    let call = Call {
        program: MUSE,
        name: ProgramName::new("muse.turn")?,
        input,
        origin: NativeOrigin::new("example.muse_turn")?,
        key: 1,
    };
    let mut harness = SeededJournal::at(&args.journal, [batch]).boot_with_argv(cli);

    let code = match harness.call_within(&call, PATIENCE) {
        CallOutcome::Transition { seq, transition, .. } => {
            writeln!(out, "outcome: transition at seq {seq}")?;
            let journal = Journal::open(&args.journal)?;
            let result =
                journal.get::<TurnResult>(&transition.result)?.ok_or("the transition cites no stored turn result")?;
            writeln!(out, "status: {}", result.status().get())?;
            report(&mut out, &journal, result.outcome())?;
            writeln!(out, "body digest: {}", result.body().digest())?;
            ExitCode::SUCCESS
        }
        CallOutcome::Fault { seq, fault, .. } => {
            writeln!(out, "outcome: fault at seq {seq}")?;
            writeln!(out, "reason: {:?}", fault.reason)?;
            ExitCode::SUCCESS
        }
        CallOutcome::Refused { reason, .. } => {
            writeln!(out, "outcome: refused before anything was recorded: {reason:?}")?;
            ExitCode::FAILURE
        }
    };
    writeln!(out, "journal: {}", args.journal.display())?;
    Ok(code)
}

/// A batch holding the bundle under [`MUSE`], the conversation's texts, and the turn input, plus the input's digest.
fn seed(args: &Args, wasm: &[u8]) -> Result<(Batch, Digest), Box<dyn Error>> {
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(wasm).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MUSE), bundle), None)?;

    let conversation = args.developer.iter().map(|text| (Role::Developer, text)).chain([(Role::User, &args.prompt)]);
    let items = conversation.map(|(role, text)| TurnItem::new(role, batch.stage_text(text))).collect();
    let input = TurnInput::new(
        Endpoint::new(args.endpoint.clone())?,
        ModelName::new(args.model.clone())?,
        TurnItems::new(items)?,
        OutputBudget::new(args.max_output_tokens)?,
        args.reasoning.into(),
    );
    let input = batch.stage_encoded(&input)?.digest();
    Ok((batch, input))
}

/// Print the outcome variant, its text, and its usage counts.
fn report(out: &mut impl Write, journal: &Journal, outcome: &TurnOutcome) -> Result<(), Box<dyn Error>> {
    match outcome {
        TurnOutcome::Completed { text, usage } => {
            writeln!(out, "turn: Completed")?;
            writeln!(out, "text: {}", read_text(journal, *text)?)?;
            write_usage(out, usage)?;
        }
        TurnOutcome::Incomplete { text, reason, usage } => {
            writeln!(out, "turn: Incomplete ({})", reason.as_str())?;
            writeln!(out, "text: {}", read_text(journal, *text)?)?;
            write_usage(out, usage)?;
        }
        TurnOutcome::Declined { refusal, usage } => {
            writeln!(out, "turn: Declined")?;
            writeln!(out, "refusal: {}", read_text(journal, *refusal)?)?;
            write_usage(out, usage)?;
        }
        TurnOutcome::Rejected => writeln!(out, "turn: Rejected")?,
        TurnOutcome::Unreadable => writeln!(out, "turn: Unreadable")?,
    }
    Ok(())
}

/// The stored text `text` cites.
fn read_text(journal: &Journal, text: Ref<Utf8Text>) -> Result<String, Box<dyn Error>> {
    let (_, payload) =
        journal.get_bytes(&text.digest())?.ok_or("the result cites a text the journal does not store")?;
    Ok(String::from_utf8(payload)?)
}

/// Print the four usage counts the vendor reported.
fn write_usage(out: &mut impl Write, usage: &TurnUsage) -> io::Result<()> {
    writeln!(
        out,
        "usage: input {} (cached {}), output {} (reasoning {})",
        usage.input_tokens(),
        usage.cached_input_tokens(),
        usage.output_tokens(),
        usage.reasoning_tokens()
    )
}
