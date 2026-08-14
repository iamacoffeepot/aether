//! `cargo xtask bloom roll` — the ADR-0186 day roll as one command.
//!
//! The roll is mechanical: quiesce, sync the day back to main, cut tomorrow
//! from post-sync main, repoint. What the sequence needs is refusal rather than
//! judgement — every precondition is checked before anything moves, so a roll
//! that cannot finish has not started, and the steps that live outside this
//! repository are printed rather than assumed. The failure modes worth designing
//! against here are the quiet ones: a cut taken from a stale ref and a rebuild
//! that lands in a different target directory both succeed, and both cost a day
//! of blooms before anyone reads a log.

mod cut;
mod day;
mod preconditions;
mod shell;
mod sync;

use anyhow::{Result, bail};
use clap::Args;

use self::day::Day;
use self::shell::Shell;
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

    /// The remote the cut is taken from and pushed to.
    #[arg(long, default_value = "origin")]
    remote: String,
}

pub fn run(client: &Client<'_>, args: &RollArgs) -> Result<String> {
    roll(&client.view()?, &shell::Host, args)
}

fn roll(view: &ViewDocument, shell: &impl Shell, args: &RollArgs) -> Result<String> {
    let from = sync_from(&args.from)?;
    preconditions::screen(view, shell, &args.date, &args.remote)?;
    let synced = sync::merge(shell, &from)?;
    cut::create(shell, &args.remote, &args.date)?;
    Ok(handoff(&args.date, &synced))
}

/// The day branch the sync-back runs from, normalized to the bare branch name
/// `gh` and `git` take.
///
/// The operator reads the day off the coordinator's own boot-resolved knob,
/// which is spelled `refs/heads/…` where a pull request's head and a push
/// refspec both want the branch alone, and a qualified name in either position
/// addresses something that is not there.
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
        "synced the day back as #{synced} and rolled onto {branch}.\n\
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
    use super::{Day, RollArgs, roll, sync_from};
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};

    fn drained_view() -> ViewDocument {
        ViewDocument {
            mainline: DigestHex::from_bytes([1; 32]),
            observed: DigestHex::from_bytes([2; 32]),
            blooms: vec![BloomView {
                id: DigestHex::from_bytes([3; 32]),
                status: BloomStatus::Landed,
                superseded_by: None,
                members: vec![MemberView {
                    workpiece: "issue-4945".to_owned(),
                    scope_revision: DigestHex::from_bytes([7; 32]),
                }],
            }],
        }
    }

    fn args(from: &str) -> RollArgs {
        RollArgs {
            date: Day::parse("2026-08-15").expect("a well-formed day"),
            from: from.to_owned(),
            remote: "origin".to_owned(),
        }
    }

    fn green() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.starts_with("gh api") => Run::ok("true"),
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn a_green_roll_syncs_back_before_it_cuts_tomorrow() {
        let shell = green();

        roll(&drained_view(), &shell, &args("bloomery/daily/2026-08-14")).expect("a drained day rolls");

        let calls = shell.calls();
        let merged = calls.iter().position(|line| line.starts_with("gh pr merge")).expect("the day syncs back");
        let cut = calls.iter().position(|line| line.starts_with("git branch")).expect("tomorrow is cut");
        assert!(merged < cut, "the cut is taken after the sync-back merges: {calls:?}");
    }

    // Tripwire: the printed knob has to be the one the coordinator reads
    // (`AETHER_BLOOMERY_MAINLINE_REF` in `aether-chassis-bloomery`'s config) in
    // the fully-qualified spelling that config normalizes from. This line is the
    // whole interface between the roll and the host it cannot reach into, so a
    // drifted name or a bare branch is a repoint that silently keeps yesterday's
    // ref for a day.
    #[test]
    fn the_handoff_prints_the_repoint_line_verbatim() {
        let handoff = roll(&drained_view(), &green(), &args("bloomery/daily/2026-08-14")).expect("a drained day rolls");

        assert!(
            handoff.contains("AETHER_BLOOMERY_MAINLINE_REF=refs/heads/bloomery/daily/2026-08-15"),
            "the repoint line is pasteable: {handoff}"
        );
        assert!(handoff.contains("restart"), "the restart is named rather than skipped: {handoff}");
    }

    // Tripwire: a refused roll has moved nothing. The screen runs against the
    // live view and the host before the sync-back exists, so an undrained day
    // costs a re-run rather than a half-rolled repository.
    #[test]
    fn a_refused_roll_touches_neither_the_pull_request_nor_the_branch() {
        let mut view = drained_view();
        view.blooms[0].status = BloomStatus::Sealed;
        let shell = green();

        roll(&view, &shell, &args("bloomery/daily/2026-08-14")).expect_err("an undrained day is refused");

        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.starts_with("gh pr")), "no pull request is opened or merged: {calls:?}");
        assert!(!calls.iter().any(|line| line.starts_with("git branch")), "tomorrow is not cut: {calls:?}");
        assert!(!calls.iter().any(|line| line.starts_with("git push")), "nothing is pushed: {calls:?}");
    }

    // The operator copies the day off the coordinator's `AETHER_BLOOMERY_MAINLINE_REF`,
    // which is qualified; `gh pr create --head` and a push refspec both want the
    // branch alone, so a ref carried through verbatim addresses nothing and the
    // roll dies between the sync-back and the cut.
    #[test]
    fn the_day_branch_is_taken_bare_from_either_spelling() {
        let day = "bloomery/daily/2026-08-14";
        for named in [format!("refs/heads/{day}"), format!("heads/{day}"), format!("  {day}  ")] {
            assert_eq!(sync_from(&named).expect("the flag names the day"), day);
        }
        assert!(sync_from("   ").is_err(), "a blank --from is a refusal, not a roll of `main`");
    }
}
