//! `cargo xtask bloom roll` — the ADR-0186 day roll as one command, as
//! amended by ADR-0203.
//!
//! The roll is mechanical: quiesce, linearize the day onto fleet main under
//! the coverage-map barrier, compare-and-swap `refs/heads/main`, cut tomorrow
//! from that advanced main, repoint. GitHub is a best-effort replica after
//! the advance, never a gate. What the sequence needs is refusal rather than
//! judgement — every precondition is checked before anything moves, so a roll
//! that cannot finish has not started, and the steps that live outside this
//! repository are printed rather than assumed. The failure modes worth designing
//! against here are the quiet ones: a cut taken from a stale replica and a
//! rebuild that lands in a different target directory both succeed, and both
//! cost a day of blooms before anyone reads a log.

mod coverage;
mod cut;
mod day;
mod preconditions;
mod replica;
mod shell;
mod sync;

use aether_bloomery_git::DayCoverage;
use anyhow::{Result, bail};
use clap::Args;

use self::day::Day;
use self::shell::{Repo, Shell};
use crate::bloom::client::Client;
use crate::bloom::dto::ViewDocument;

/// The branch the day syncs back onto. Bloomery's mainline moves day to day;
/// what it returns to does not.
const MAIN: &str = "main";

/// Drive one ADR-0186 day roll.
#[derive(Args, Debug)]
pub struct RollArgs {
    /// The day tomorrow's branch is cut for, as `YYYY-MM-DD`.
    #[arg(long, value_parser = Day::parse)]
    date: Day,

    /// The day branch to sync back, as the coordinator's mainline ref names it.
    #[arg(long)]
    from: String,

    /// The GitHub replica the advanced main and the new daily are pushed to.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// The fleet repository whose refs the roll reads and writes — the
    /// coordinator's `AETHER_BLOOMERY_AUTHORITY_REPO`, or any worktree of it.
    /// Defaults to the current directory, which is right only when the roll is
    /// driven from inside the fleet repository.
    #[arg(long, default_value = ".")]
    repo: String,

    /// Replay the day commit by commit instead of syncing its tree as one
    /// commit. Keeps the day's authored history, and is only available to a day
    /// with no bloom folds in it — a fold-bearing day cannot be replayed
    /// linearly at all (#5414).
    #[arg(long)]
    replay: bool,
}

pub fn run(client: &Client<'_>, args: &RollArgs) -> Result<String> {
    let view = client.view()?;
    let coverage = match client.journal() {
        Ok(journal) => coverage::day_coverage(&view, &journal),
        Err(error) => DayCoverage::hold(error.to_string()),
    };

    roll(&view, &shell::Host, &coverage, args)
}

fn roll(view: &ViewDocument, shell: &impl Shell, coverage: &DayCoverage, args: &RollArgs) -> Result<String> {
    let from = sync_from(&args.from)?;
    let repo = Repo::new(args.repo.clone());
    preconditions::screen(view, shell, &repo, &args.date, &args.remote)?;
    let synced = sync::merge(shell, &repo, &args.remote, &from, coverage, args.replay)?;
    cut::create(shell, &repo, &args.remote, &args.date)?;
    Ok(handoff(&args.date, &synced))
}

/// The day branch the sync-back runs from, normalized to the bare branch name
/// `git` takes.
///
/// The operator reads the day off the coordinator's own boot-resolved knob,
/// which is spelled `refs/heads/…` where a push refspec and a local ref both
/// want the branch alone, and a qualified name in either position addresses
/// something that is not there.
fn sync_from(named: &str) -> Result<String> {
    let named = named.trim();
    if named.is_empty() {
        bail!("--from needs the day branch to sync back, e.g. --from bloomery/daily/2026-08-14");
    }

    let branch = named.strip_prefix("refs/").unwrap_or(named);
    Ok(branch.strip_prefix("heads/").unwrap_or(branch).to_owned())
}

/// The two steps the command cannot perform, printed verbatim.
///
/// The mainline ref is boot configuration on the host, outside this repository
/// and outside this process, so the roll ends by handing the operator the exact
/// line to set rather than editing an environment file it does not own.
fn handoff(day: &Day, synced: &str) -> String {
    format!(
        "synced the day onto main as {synced} and rolled onto {branch}.\n\
         \n\
         two steps stay host-side, because the coordinator's mainline ref is boot configuration\n\
         outside this repository:\n\
         \n\
         \x20 1. repoint the coordinator's boot environment:\n\
         \n\
         \x20      AETHER_BLOOMERY_MAINLINE_REF={mainline_ref}\n\
         \n\
         \x20 2. rebuild the coordinator into the target directory the running unit launches from,\n\
         \x20    then restart it so boot resolves the new ref. A rebuild into a different\n\
         \x20    CARGO_TARGET_DIR leaves the unit on yesterday's binary and says nothing.\n",
        branch = day.branch(),
        mainline_ref = day.mainline_ref(),
    )
}

#[cfg(test)]
mod tests {
    use aether_bloomery::BloomStatus;

    use super::shell::Run;
    use super::shell::fake::Fake;
    use aether_bloomery_git::DayCoverage;

    use super::{Day, RollArgs, roll, sync_from};
    use crate::bloom::dto::{ViewDocument, test_bloom, test_member, test_view};
    use aether_bloomery::Digest;

    fn drained_view() -> ViewDocument {
        test_view(
            Digest::from_bytes([1; 32]),
            Digest::from_bytes([2; 32]),
            vec![test_bloom(
                Digest::from_bytes([3; 32]),
                BloomStatus::Landed,
                vec![test_member("issue-4945", Digest::from_bytes([7; 32]))],
            )],
        )
    }

    fn args(from: &str) -> RollArgs {
        RollArgs {
            date: Day::parse("2026-08-15").expect("a well-formed day"),
            from: from.to_owned(),
            remote: "origin".to_owned(),
            repo: "/mnt/dev/bloomery/fleet.git".to_owned(),
            replay: false,
        }
    }

    fn green() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("commit-tree") => Run::ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains("refs/heads/main") => {
                Run::ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            }
            line if line.contains("rev-parse") => Run::ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn a_green_roll_advances_fleet_main_before_it_cuts_tomorrow() {
        let shell = green();

        roll(&drained_view(), &shell, &DayCoverage::green(), &args("bloomery/daily/2026-08-14"))
            .expect("a drained day rolls");

        let calls = shell.calls();
        let advanced = calls.iter().position(|line| line.contains("update-ref")).expect("the day advances onto main");
        let cut = calls.iter().position(|line| line.contains(" branch ")).expect("tomorrow is cut");
        assert!(advanced < cut, "the cut is taken after fleet main advances: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.contains("FETCH_HEAD") || line.contains("git fetch")),
            "the cut is not taken from a GitHub fetch: {calls:?}"
        );
    }

    // Tripwire: the printed knob has to be the one the coordinator reads
    // (`AETHER_BLOOMERY_MAINLINE_REF` in `aether-chassis-bloomery`'s config) in
    // the fully-qualified spelling that config normalizes from. This line is the
    // whole interface between the roll and the host it cannot reach into, so a
    // drifted name or a bare branch is a repoint that silently keeps yesterday's
    // ref for a day.
    #[test]
    fn the_handoff_prints_the_repoint_line_verbatim() {
        let handoff = roll(&drained_view(), &green(), &DayCoverage::green(), &args("bloomery/daily/2026-08-14"))
            .expect("a drained day rolls");

        assert!(
            handoff.contains("AETHER_BLOOMERY_MAINLINE_REF=refs/heads/bloomery/daily/2026-08-15"),
            "the repoint line is pasteable: {handoff}"
        );
        assert!(handoff.contains("restart"), "the restart is named rather than skipped: {handoff}");
    }

    // Tripwire: a refused roll has moved nothing. The screen runs against the
    // live view and the host before fleet main is swapped, so an undrained day
    // costs a re-run rather than a half-rolled repository. The screen's own
    // `--dry-run` replica probe is exempt because it writes nothing; every other
    // push is a write and must not appear.
    #[test]
    fn a_refused_roll_touches_neither_main_nor_the_branch() {
        let mut view = drained_view();
        view.blooms[0].status = BloomStatus::Sealed;
        let shell = green();

        roll(&view, &shell, &DayCoverage::green(), &args("bloomery/daily/2026-08-14"))
            .expect_err("an undrained day is refused");

        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.contains("update-ref")), "fleet main is not swapped: {calls:?}");
        assert!(!calls.iter().any(|line| line.contains(" branch ")), "tomorrow is not cut: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.contains(" push ") && !line.contains("--dry-run")),
            "nothing is pushed: {calls:?}"
        );
    }

    #[test]
    fn a_held_coverage_map_is_a_nonzero_refusal() {
        let shell = green();

        let refusal = roll(
            &drained_view(),
            &shell,
            &DayCoverage::hold("red test crate::day_head"),
            &args("bloomery/daily/2026-08-14"),
        )
        .expect_err("a non-green map refuses the roll")
        .to_string();

        assert!(refusal.contains("not green"), "the refusal names the coverage bar: {refusal}");
        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.contains("update-ref")), "a held map does not swap main: {calls:?}");
        assert!(!calls.iter().any(|line| line.contains(" branch ")), "tomorrow is not cut: {calls:?}");
    }

    // The operator copies the day off the coordinator's `AETHER_BLOOMERY_MAINLINE_REF`,
    // which is qualified; a local ref and a push refspec both want the branch
    // alone, so a ref carried through verbatim addresses nothing and the roll
    // dies between the advance and the cut.
    #[test]
    fn the_day_branch_is_taken_bare_from_either_spelling() {
        let day = "bloomery/daily/2026-08-14";
        for named in [format!("refs/heads/{day}"), format!("heads/{day}"), format!("  {day}  ")] {
            assert_eq!(sync_from(&named).expect("the flag names the day"), day);
        }
        assert!(sync_from("   ").is_err(), "a blank --from is a refusal, not a roll of `main`");
    }
}
