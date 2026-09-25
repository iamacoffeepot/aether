//! Crash recovery for real: fork `aether-bloomery` through the hub with restart
//! supervision armed, SIGKILL it while a program request is in flight, and
//! read back the journal the restarted engine recovered.
//!
//! The in-process scenarios never kill anything, and the driver's sans-io
//! `restart.rs` restarts the core over the same journal truth without a
//! process dying. This scenario is the one place a real process death is
//! followed by a real reopen of the file it was writing.
//!
//! The kill needs the engine's pid, which no fleet kind reports. The binary
//! the hub forks is a three-line shell wrapper: it answers `--describe` with
//! the real binary's manifest, and otherwise logs its pid and `exec`s the real
//! binary, which keeps the pid. Unix only, for the wrapper and the kill.

#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    AwaitProcessed, Call, CallOutcome, Digest, Fault, FaultReason, Head, NativeOrigin, OpaqueBytes, Processed,
    ProgramName, ProgramRef, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, Utf8Text,
};
use aether_data::{EngineId, Kind};
use aether_fleet::RestartPolicy;
use aether_harness_bloomery::{Record, SeededJournal};
use aether_harness_fleet::{FleetHarness, poll_until};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_rpc::ReplyEnvelope;

/// The bundle driver's canonical path. It is an instanced root, so no short
/// `root/:disc` path anchors on it.
const DRIVER: &str = "aether.bloomery.driver:driver";

/// The `program` head the seed binds to the fixture bundle.
const PROGRAM: Head<OpaqueBytes> = Head::new("program");

/// Local mirror of the fixture's `test.program.summarize.input`: same kind
/// name, same shape, so it encodes to the same digest the guest expects.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

/// Local mirror of the fixture's `test.program.stall.input`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.stall.input")]
struct StallInput {
    marker: u32,
}

/// Write the wrapper the hub forks in place of the real binary: `--describe`
/// passes through so the store ingests the genuine manifest, and every other
/// invocation appends its pid to `pids` before `exec`ing the real binary with
/// the fleet's argv.
fn write_wrapper(dir: &Path, real: &Path, pids: &Path) -> PathBuf {
    let wrapper = dir.join("aether-bloomery-wrapper");
    let (real, pids) = (real.display(), pids.display());
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--describe\" ]; then exec '{real}' --describe; fi\n\
         echo $$ >> '{pids}'\n\
         exec '{real}' \"$@\"\n"
    );
    fs::write(&wrapper, script).expect("write the wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("make the wrapper executable");
    wrapper
}

/// Ask the driver on `engine` for `AwaitProcessed { through }` and decode its
/// answer. This is the recovery barrier: the driver answers only after its
/// startup recovery has committed and no request at or below `through` is
/// outstanding.
fn await_driver(fleet: &mut FleetHarness, engine: EngineId, through: u64) -> Processed {
    single(&fleet.send(engine, DRIVER, &AwaitProcessed { through }))
}

/// Send `request` to the driver on `engine`, wait for the call to settle, and
/// decode its one outcome.
fn call(fleet: &mut FleetHarness, engine: EngineId, request: &Call) -> CallOutcome {
    single(&fleet.send(engine, DRIVER, request))
}

/// The one reply in `replies`, decoded as `K`.
fn single<K: Kind>(replies: &[ReplyEnvelope]) -> K {
    match replies {
        [reply] => K::decode_from_bytes(&reply.payload).unwrap_or_else(|| panic!("undecodable {}", K::NAME)),
        other => panic!("expected one {} reply, got {}", K::NAME, other.len()),
    }
}

/// A native call of program `name` over `input`, keyed `key` within `origin`.
fn request(name: &ProgramName, input: Digest, origin: &NativeOrigin, key: u64) -> Call {
    Call { program: PROGRAM, name: name.clone(), input, origin: origin.clone(), key }
}

/// Whether the journal root at `journal` has committed through `seq`. The
/// engine holds the root's lock and its log open in WAL mode, so this reads
/// through a lock-free [`JournalReader`], and an open or read error reads as
/// not yet.
fn head_reached(journal: &Path, seq: Seq) -> bool {
    JournalReader::open(journal).and_then(|journal| journal.head()).is_ok_and(|head| head >= seq)
}

/// SIGKILL `pid` once the journal has committed through `seq`, or once the
/// poll budget runs out, so the caller blocked on the in-flight call is never
/// stranded. Returns whether `seq` was seen before the kill.
fn kill_when_recorded(journal: &Path, seq: Seq, pid: u32) -> bool {
    let recorded = poll_until(|| head_reached(journal, seq));
    let status = Command::new("kill").args(["-KILL", &pid.to_string()]).status().expect("run kill");
    assert!(status.success(), "kill -KILL {pid} failed: {status}");
    recorded
}

#[test]
fn a_killed_engine_restarts_over_its_journal_and_faults_the_in_flight_call_interrupted() -> Result<(), Box<dyn Error>> {
    // Catches: a journal that does not reopen after a kill mid-WAL, so the
    // successor never mounts; a restart that loses `--bloomery-journal` or
    // opens another file, so the head is not 5; an in-flight request that is
    // re-run instead of faulted, which wedges `AwaitProcessed` and queues key 3
    // behind it; an `Interrupted` fault with the wrong cause, or recorded twice;
    // a dedup index not rebuilt from the journal, so key 1 runs again; a caller
    // of an in-flight call that hangs instead of getting an error; and any
    // record beyond the literal list.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path)?;
    let mut seed = Batch::new();
    let bundle = seed.stage_bytes(&wasm);
    seed.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROGRAM), bundle.digest()), None)?;
    let text = seed.stage_text("hello");
    let summarize_input = seed.stage_encoded(&SummarizeInput { text })?.digest();
    let stall_input = seed.stage_encoded(&StallInput { marker: 1 })?.digest();

    let seeded = SeededJournal::new([seed]);
    let journal = seeded.journal_path();
    let scratch = journal.parent().expect("the journal sits in the seed's scratch directory");
    let pids = scratch.join("pids");
    let wrapper = write_wrapper(scratch, Path::new(env!("CARGO_BIN_EXE_aether-bloomery")), &pids);
    let mut fleet = FleetHarness::start_restarting(RestartPolicy {
        backoff: Duration::from_millis(250),
        burst_limit: 1,
        burst_window: Duration::from_mins(5),
    });
    let engine = fleet.spawn_binary(&wrapper, vec!["--bloomery-journal".to_owned(), journal.display().to_string()]);
    let first_pid: u32 = fs::read_to_string(&pids)?.lines().next().expect("the first engine logged its pid").parse()?;

    let origin = NativeOrigin::new("test.crash")?;
    let summarize_name = ProgramName::new("test.program.summarize")?;
    let stall_name = ProgramName::new("test.program.stall")?;
    let summarize_program = ProgramRef::new(bundle.digest(), summarize_name.clone());
    let stall_program = ProgramRef::new(bundle.digest(), stall_name.clone());

    assert_eq!(await_driver(&mut fleet, engine, 1), Processed { head: 1 });

    let first = call(&mut fleet, engine, &request(&summarize_name, summarize_input, &origin, 1));
    let CallOutcome::Transition { key: 1, seq: 3, transition } = first.clone() else {
        panic!("expected the summarize Transition at seq 3, got {first:?}");
    };
    seeded.assert_appended(
        Seq(1),
        &[
            Record::equal(
                None,
                Requested {
                    program: summarize_program.clone(),
                    input: summarize_input,
                    source: RequestSource::Native { origin: origin.clone(), key: 1 },
                },
            ),
            Record::equal(Some(Seq(2)), transition),
        ],
    );

    let stall = request(&stall_name, stall_input, &origin, 2);
    let (in_flight, recorded_before_kill) = thread::scope(|scope| {
        let killer = scope.spawn(move || kill_when_recorded(journal, Seq(4), first_pid));
        let in_flight = fleet.try_send(engine, DRIVER, &stall);
        (in_flight, killer.join().expect("the killer thread finished"))
    });
    assert!(recorded_before_kill, "the stall request's Requested committed at seq 4 before the kill");
    in_flight.expect_err("the caller of the in-flight call gets an error when its engine dies");

    let successor = fleet.await_restart(engine);
    assert_eq!(await_driver(&mut fleet, successor, 4), Processed { head: 5 });
    let interrupted = Fault { program: stall_program.clone(), input: stall_input, reason: FaultReason::Interrupted };
    seeded.assert_appended(
        Seq(3),
        &[
            Record::equal(
                None,
                Requested {
                    program: stall_program,
                    input: stall_input,
                    source: RequestSource::Native { origin: origin.clone(), key: 2 },
                },
            ),
            Record::equal(Some(Seq(4)), interrupted.clone()),
        ],
    );

    assert_eq!(call(&mut fleet, successor, &stall), CallOutcome::Fault { key: 2, seq: 5, fault: interrupted });
    assert_eq!(seeded.head(), Seq(5), "the interrupted request is answered from its record, not run again");

    assert_eq!(call(&mut fleet, successor, &request(&summarize_name, summarize_input, &origin, 1)), first);
    assert_eq!(seeded.head(), Seq(5), "the successor answers a repeated key from the record");

    let fresh = call(&mut fleet, successor, &request(&summarize_name, summarize_input, &origin, 3));
    let CallOutcome::Transition { key: 3, seq: 7, transition } = fresh else {
        panic!("expected a fresh Transition at seq 7, got {fresh:?}");
    };
    seeded.assert_appended(
        Seq(5),
        &[
            Record::equal(
                None,
                Requested {
                    program: summarize_program,
                    input: summarize_input,
                    source: RequestSource::Native { origin, key: 3 },
                },
            ),
            Record::equal(Some(Seq(6)), transition),
        ],
    );
    Ok(())
}
