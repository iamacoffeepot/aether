//! ADR-0237 step 0: one Sampled `Process` program runs `docker run --rm --network none ... cargo clippy` through the
//! bloomery chassis, and the journal records the exit code and output.
//!
//! Needs Docker, the spike bundle wasm, and three env vars: `SPIKE_WORK_DIR` (the crate to lint, bind-mounted at
//! `/work` as decision 10 states), `SPIKE_IMAGE` (an image with cargo + clippy), and `SPIKE_USER` (`uid:gid`).

use std::error::Error;
use std::fs;
use std::time::Instant;

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, Head, NativeOrigin, OpaqueBytes, ProgramName, ProgramRef, RecordedHead, RecordedHeadMove, Ref,
    RequestSource, Requested, Transition, Utf8Text,
};
use aether_chassis_bloomery::BloomeryCli;
use aether_harness_bloomery::{Record, SeededJournal};
use aether_harness_substrate::test_helpers::require_wasm;
use clap::Parser;

/// Local mirror of the bundle's `spike.workspace.clippy.input`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "spike.workspace.clippy.input")]
struct ClippyInput {
    binary: Ref<Utf8Text>,
    argv: Ref<Utf8Text>,
    env: Ref<Utf8Text>,
    timeout_millis: u32,
}

/// Local mirror of the bundle's `spike.workspace.clippy.result`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "spike.workspace.clippy.result")]
struct ClippyResult {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: Ref<OpaqueBytes>,
    stderr: Ref<OpaqueBytes>,
}

const SPIKE: Head<OpaqueBytes> = Head::new("spike");

#[allow(clippy::disallowed_methods)] // spike driver input, not capability config
fn var(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("set {key}"))
}

#[test]
#[ignore = "needs docker; run explicitly"]
fn one_sampled_process_program_runs_containerized_clippy() -> Result<(), Box<dyn Error>> {
    let wasm = fs::read(require_wasm("spike_workspace_step0_program").expect("spike bundle wasm built"))?;
    let (work, image, user) = (var("SPIKE_WORK_DIR"), var("SPIKE_IMAGE"), var("SPIKE_USER"));
    let argv = [
        "run",
        "--rm",
        "--network",
        "none",
        "--read-only",
        "--user",
        &user,
        "--tmpfs",
        "/tmp:exec",
        "--tmpfs",
        "/work/target:exec",
        "-e",
        "PATH=/usr/local/cargo/bin:/usr/bin:/bin",
        "-e",
        "RUSTUP_HOME=/usr/local/rustup",
        "-e",
        "CARGO_HOME=/tmp/cargo-home",
        "-e",
        "SOURCE_DATE_EPOCH=0",
        "-v",
        &format!("{work}:/work"),
        "-w",
        "/work",
        &image,
        "cargo",
        "clippy",
        "--offline",
        "--",
        "-D",
        "warnings",
    ]
    .join("\n");

    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm).digest();
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&SPIKE), bundle), None)?;
    let input = ClippyInput {
        binary: seed.stage_text("docker"),
        argv: seed.stage_text(&argv),
        env: seed.stage_text("HOME=/tmp"),
        timeout_millis: 25_000,
    };
    let input = seed.stage_encoded(&input)?.digest();

    let cli = BloomeryCli::try_parse_from(["aether-bloomery", "--process-allowlist", "docker=/usr/bin/docker"])?;
    let booted = Instant::now();
    let mut harness = SeededJournal::new([seed]).boot_with_argv(cli);
    eprintln!("spike: chassis boot millis={}", booted.elapsed().as_millis());

    let origin = NativeOrigin::new("spike.step0")?;
    let name = ProgramName::new("spike.workspace.clippy")?;
    let call = Call { program: SPIKE, name: name.clone(), input, origin: origin.clone(), key: 1 };
    let started = Instant::now();
    let outcome = harness.call(&call);
    eprintln!("spike: call millis={}", started.elapsed().as_millis());
    eprintln!("spike: outcome={outcome:?}");
    let CallOutcome::Transition { key: 1, seq, transition } = outcome else {
        panic!("expected a Transition, got {outcome:?}");
    };

    let requested =
        Requested { program: ProgramRef::new(bundle, name), input, source: RequestSource::Native { origin, key: 1 } };
    harness.assert_appended(
        Seq(1),
        &[Record::equal(None, requested), Record::equal::<Transition>(Some(Seq(2)), transition.clone())],
    );

    let journal = Journal::open(harness.journal_path())?;
    let result = journal.get::<ClippyResult>(&transition.result)?.expect("the transition cites a stored result");
    let text = |r: &Ref<OpaqueBytes>| {
        journal
            .get_bytes(&r.digest())
            .ok()
            .flatten()
            .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|| "<not stored>".to_owned())
    };
    eprintln!("spike: transition seq={seq:?} result={result:?}");
    eprintln!("spike: journal head={:?}", journal.head()?);
    eprintln!("spike: stdout=<<{}>>", text(&result.stdout));
    eprintln!("spike: stderr=<<{}>>", text(&result.stderr));
    Ok(())
}
