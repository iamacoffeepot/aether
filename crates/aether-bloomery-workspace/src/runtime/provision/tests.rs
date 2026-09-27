//! Provisioning's decisions without a daemon or a context: core lists, run
//! keys, estimates, the budget, and FIFO admission.

use std::error::Error;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use aether_bloomery_kinds::{Detail, Digest, Ref};

use super::budget::Budget;
use super::cpuset::{CpuSet, CpuSetError};
use super::estimate::{Amounts, Estimates, Headroom, MIN_DEADLINE, MIN_MEMORY_BYTES};
use super::key::RunKey;
use super::queue::Admission;
use crate::runtime::run::{Allotment, Observed};
use crate::{
    EnvVar, Mount, Mounts, Network, Outcome, Refusal, Resource, Run, RunResult, Scratch, Step, Steps, ToolName,
    TreePath,
};

type TestResult = Result<(), Box<dyn Error>>;

const GIB: u64 = 1 << 30;
const MINUTE: Duration = Duration::from_mins(1);

fn cpus(list: &str) -> Result<CpuSet, Box<dyn Error>> {
    Ok(CpuSet::parse(list)?)
}

fn amounts(cores: u32, memory_bytes: u64, deadline: Duration) -> Result<Amounts, Box<dyn Error>> {
    Ok(Amounts {
        cores: NonZeroU32::new(cores).ok_or("zero cores")?,
        memory_bytes: NonZeroU64::new(memory_bytes).ok_or("zero memory")?,
        deadline,
    })
}

/// An estimate table with 1 core, 1 GiB, and 30 minutes for an unseen key,
/// under a 4-core, 8 GiB, 4-hour ceiling.
fn estimates(headroom: u32) -> Result<Estimates, Box<dyn Error>> {
    Ok(Estimates::new(amounts(1, GIB, 30 * MINUTE)?, Headroom::new(headroom)?, amounts(4, 8 * GIB, 240 * MINUTE)?))
}

fn allotment(given: &Amounts) -> Result<Allotment, Box<dyn Error>> {
    Ok(Allotment { cpus: cpus("0")?, memory_bytes: given.memory_bytes.get(), deadline: given.deadline })
}

fn ok() -> RunResult {
    RunResult::Ok(Outcome { steps: Vec::new(), tree: Ref::from_digest(Digest::from_bytes([0; 32])) })
}

fn seen(peak_memory_bytes: u64, wall: Duration) -> Observed {
    Observed { peak_memory_bytes: Some(peak_memory_bytes), wall: Some(wall) }
}

fn step(tool: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Step, Box<dyn Error>> {
    Ok(Step {
        tool: ToolName::new(tool)?,
        args: args.iter().map(|&arg| arg.to_owned()).collect(),
        env: env.iter().map(|&(key, value)| EnvVar::new(key, value)).collect::<Result<_, _>>()?,
        stdin: None,
    })
}

/// A run of `steps` in the environment whose digest is all `environment`.
fn run(environment: u8, steps: Vec<Step>) -> Result<Run, Box<dyn Error>> {
    Ok(Run {
        tree: Ref::from_digest(Digest::from_bytes([1; 32])),
        environment: Ref::from_digest(Digest::from_bytes([environment; 32])),
        mounts: Mounts::new(Vec::new())?,
        steps: Steps::new(steps)?,
        scratch: Scratch::new(Vec::new())?,
        network: Network::Off,
    })
}

fn key(run: &Run) -> RunKey {
    RunKey::of(run)
}

#[test]
fn a_cpuset_list_parses_merged_renders_compact_and_refuses_what_docker_would() -> TestResult {
    // Catches a parser that pins the wrong cores: a range read half-open or
    // not expanded, overlaps counted twice, or a malformed list accepted.
    let set = cpus("6,0-3")?;
    assert_eq!(set.docker_list(), "0-3,6");
    assert_eq!(set.count().get(), 5);
    assert_eq!(cpus("3,0-2,1,5-5")?.docker_list(), "0-3,5");

    assert_eq!(CpuSet::parse(""), Err(CpuSetError::Empty));
    assert_eq!(CpuSet::parse("1,,2"), Err(CpuSetError::Empty));
    assert_eq!(CpuSet::parse("3-1"), Err(CpuSetError::Reversed { low: 3, high: 1 }));
    assert_eq!(CpuSet::parse("a"), Err(CpuSetError::NotANumber));
    assert_eq!(CpuSet::parse(" 1"), Err(CpuSetError::NotANumber));
    assert_eq!(CpuSet::parse("0-1024"), Err(CpuSetError::AboveMax));
    Ok(())
}

#[test]
fn the_run_key_covers_the_environment_and_each_steps_tool_args_and_env_and_nothing_else() -> TestResult {
    // Catches a dropped field, which merges the estimates of runs that do
    // different work, an extra one, which splits the estimates of runs that
    // do the same work over different inputs, and fields run together
    // without lengths, which merges `["ab"]` with `["a", "b"]`.
    let base = run(2, vec![step("cargo", &["build", "--release"], &[("CARGO_INCREMENTAL", "0")])?])?;

    let differs = [
        run(3, base.steps.as_slice().to_vec())?,
        run(2, vec![step("rustc", &["build", "--release"], &[("CARGO_INCREMENTAL", "0")])?])?,
        run(2, vec![step("cargo", &["build", "--locked"], &[("CARGO_INCREMENTAL", "0")])?])?,
        run(2, vec![step("cargo", &["build", "--release"], &[("CARGO_INCREMENTAL", "1")])?])?,
        run(2, vec![step("cargo", &["build", "--release"], &[("CARGO_PROFILE", "0")])?])?,
        run(2, vec![step("cargo", &["build", "--release"], &[("CARGO_INCREMENTAL", "0")])?; 2])?,
    ];
    for other in &differs {
        assert_ne!(key(other), key(&base), "{other:?}");
    }
    assert_ne!(
        key(&run(2, vec![step("cargo", &["ab"], &[])?])?),
        key(&run(2, vec![step("cargo", &["a", "b"], &[])?])?)
    );

    let mut stdin = base.steps.as_slice().to_vec();
    stdin[0].stdin = Some(Ref::of_bytes(b"input"));
    let same = [
        Run { tree: Ref::from_digest(Digest::from_bytes([9; 32])), ..base.clone() },
        Run { mounts: Mounts::new(vec![Mount { at: TreePath::new("vendor")?, tree: base.tree }])?, ..base.clone() },
        Run { scratch: Scratch::new(vec![TreePath::new("target")?])?, ..base.clone() },
        Run { network: Network::On, ..base.clone() },
        Run { steps: Steps::new(stdin)?, ..base.clone() },
    ];
    for other in &same {
        assert_eq!(key(other), key(&base), "{other:?}");
    }
    Ok(())
}

#[test]
fn an_unseen_key_gets_the_defaults_and_a_seen_one_its_observation_times_headroom_floored() -> TestResult {
    // Catches the headroom left out or applied twice, an estimate that
    // ignores what was observed, and a tiny observation handed a tiny
    // allotment instead of the floor.
    let mut table = estimates(150)?;
    let big = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let tiny = key(&run(2, vec![step("cargo", &["check"], &[])?])?);
    let given = allotment(&table.amounts(&big))?;

    assert_eq!(table.amounts(&big), amounts(1, GIB, 30 * MINUTE)?);

    table.observe(&big, &given, &ok(), &seen(2 * GIB, 10 * MINUTE));
    table.observe(&tiny, &given, &ok(), &seen(1 << 20, Duration::from_secs(1)));

    assert_eq!(table.amounts(&big), amounts(1, 3 * GIB, 15 * MINUTE)?);
    assert_eq!(table.amounts(&tiny), amounts(1, MIN_MEMORY_BYTES, MIN_DEADLINE)?);
    Ok(())
}

#[test]
fn a_later_observation_blends_in_at_a_quarter() -> TestResult {
    // Catches a table that keeps only the first or only the latest
    // observation, or blends at another weight.
    let mut table = estimates(100)?;
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let given = allotment(&table.amounts(&build))?;

    table.observe(&build, &given, &ok(), &seen(4 * GIB, 40 * MINUTE));
    table.observe(&build, &given, &ok(), &seen(8 * GIB, 80 * MINUTE));

    assert_eq!(table.amounts(&build), amounts(1, 5 * GIB, 50 * MINUTE)?);
    Ok(())
}

#[test]
fn exhausting_memory_doubles_the_next_allotment_up_to_the_budget() -> TestResult {
    // Catches a retry handed the same memory that already ran out (every
    // retry would be exhausted again) and growth past the whole budget.
    let mut table = estimates(150)?;
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let mut grown = Vec::new();

    for _ in 0..4 {
        let given = allotment(&table.amounts(&build))?;
        table.observe(&build, &given, &RunResult::Exhausted(Resource::Memory), &Observed::default());
        grown.push(table.amounts(&build).memory_bytes.get());
    }

    assert_eq!(grown, [2 * GIB, 4 * GIB, 8 * GIB, 8 * GIB]);
    assert_eq!(table.amounts(&build).deadline, 30 * MINUTE, "only the resource that ran out grows");
    Ok(())
}

#[test]
fn repeated_timeouts_double_the_deadline_until_the_ceiling_and_stop_there() -> TestResult {
    // Catches a deadline that does not grow after `Exhausted(Time)` and one
    // that grows without bound, handing a hung step its cores for ever
    // longer: past 4 hours the deadline must stay at the ceiling.
    let mut table = estimates(150)?;
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let mut grown = Vec::new();

    for _ in 0..4 {
        let given = allotment(&table.amounts(&build))?;
        table.observe(&build, &given, &RunResult::Exhausted(Resource::Time), &Observed::default());
        grown.push(table.amounts(&build).deadline);
    }

    assert_eq!(grown, [60 * MINUTE, 120 * MINUTE, 240 * MINUTE, 240 * MINUTE]);
    assert_eq!(table.amounts(&build).memory_bytes.get(), GIB, "only the resource that ran out grows");
    Ok(())
}

#[test]
fn defaults_above_the_ceiling_are_clamped_to_it() -> TestResult {
    // Catches a default deadline over `max_deadline_millis`, or default
    // cores or memory over the budget, handed out as configured: the run
    // would outlive the ceiling or never fit the budget.
    let table =
        Estimates::new(amounts(8, 16 * GIB, 300 * MINUTE)?, Headroom::new(150)?, amounts(2, 4 * GIB, 240 * MINUTE)?);
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);

    assert_eq!(table.amounts(&build), amounts(2, 4 * GIB, 240 * MINUTE)?);
    Ok(())
}

#[test]
fn refused_and_failed_runs_leave_the_estimate_as_it_was() -> TestResult {
    // Catches an estimate learned from a run that never used its allotment:
    // a refusal or an executor failure says nothing about what the steps
    // need.
    let mut table = estimates(150)?;
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let given = allotment(&table.amounts(&build))?;
    let before = table.amounts(&build);

    let refused = RunResult::Refused(Refusal::EnvironmentUnavailable);
    let failed = RunResult::Failed { detail: Detail::new("the daemon hung up") };
    table.observe(&build, &given, &refused, &seen(8 * GIB, 200 * MINUTE));
    table.observe(&build, &given, &failed, &seen(8 * GIB, 200 * MINUTE));

    assert_eq!(table.amounts(&build), before);
    Ok(())
}

#[test]
fn the_budget_pins_the_lowest_free_cores_and_release_returns_them() -> TestResult {
    // Catches two runs pinned to the same core, a take that ignores the
    // free memory, and a release that loses cores or memory, so the budget
    // shrinks with every run until nothing fits.
    let mut budget = Budget::new(&cpus("0-3")?, NonZeroU64::new(4 * GIB).ok_or("zero")?);
    let two = amounts(2, GIB, MINUTE)?;

    let first = budget.take(&two).ok_or("the first run fits")?;
    let second = budget.take(&two).ok_or("the second run fits")?;
    assert_eq!((first.cpus.docker_list(), second.cpus.docker_list()), ("0-1".to_owned(), "2-3".to_owned()));
    assert!(budget.take(&amounts(1, GIB, MINUTE)?).is_none(), "no core is free");

    budget.release(&first);
    assert!(budget.take(&amounts(1, 4 * GIB, MINUTE)?).is_none(), "only 3 GiB is free");
    assert_eq!(budget.take(&two).ok_or("the released cores fit")?.cpus.docker_list(), "0-1");
    Ok(())
}

#[test]
fn a_small_run_behind_a_big_one_waits_its_turn() -> TestResult {
    // Catches backfill: a small run that fits the free budget admitted past
    // a big one waiting at the front would let a stream of small runs starve
    // the big one for ever. FIFO admits nothing past a front that does not
    // fit, at submit and at completion alike.
    let big = key(&run(2, vec![step("cargo", &["build"], &[])?])?);
    let small = key(&run(2, vec![step("cargo", &["check"], &[])?])?);
    let mut table = estimates(100)?;
    table.observe(&big, &allotment(&amounts(1, GIB, MINUTE)?)?, &ok(), &seen(6 * GIB, MINUTE));
    let mut admission = Admission::new(Budget::new(&cpus("0-3")?, NonZeroU64::new(8 * GIB).ok_or("zero")?), table);

    let running = admission.admit_now(big).ok_or("the first big run fits an idle host")?;
    assert!(admission.admit_now(big).is_none(), "the second big run does not fit beside the first");
    admission.enqueue(big, "big");
    assert!(admission.admit_now(small).is_none(), "a small run behind a waiting one waits, though it fits");
    admission.enqueue(small, "small");
    assert!(admission.next().is_none(), "nothing past a front that does not fit is admitted");

    admission.finish(&running, &ok(), &Observed::default());
    let (_, first) = admission.next().ok_or("the big run is admitted once the first finishes")?;
    let (_, second) = admission.next().ok_or("the small run fits beside it")?;
    assert_eq!((first, second), ("big", "small"));
    Ok(())
}

#[test]
fn an_allotment_larger_than_the_whole_budget_is_clamped_and_admitted_on_an_idle_host() -> TestResult {
    // Catches a run whose estimate or default exceeds the whole budget left
    // waiting for ever: on an idle host it must start, clamped to the
    // budget, even after exhaustion doubled its memory past it.
    let table = Estimates::new(amounts(8, 16 * GIB, 30 * MINUTE)?, Headroom::new(150)?, amounts(2, GIB, 240 * MINUTE)?);
    let mut admission: Admission<()> =
        Admission::new(Budget::new(&cpus("4-5")?, NonZeroU64::new(GIB).ok_or("zero")?), table);
    let build = key(&run(2, vec![step("cargo", &["build"], &[])?])?);

    let given = admission.admit_now(build).ok_or("a clamped allotment fits an idle host")?;
    assert_eq!((given.allotment.cpus.docker_list(), given.allotment.memory_bytes), ("4-5".to_owned(), GIB));

    admission.finish(&given, &RunResult::Exhausted(Resource::Memory), &Observed::default());
    let again = admission.admit_now(build).ok_or("the retry, grown past the budget, is clamped and fits")?;
    assert_eq!(again.allotment.memory_bytes, GIB);
    Ok(())
}
