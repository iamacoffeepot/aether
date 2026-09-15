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

use std::env;
use std::fmt::Write as _;

use aether_bloomery::ConfigKind;
use aether_bloomery_git::DayCoverage;
use anyhow::{Result, anyhow, bail};
use clap::Args;

use self::day::Day;
use self::shell::{Repo, Shell};
use crate::bloom::client::Client;
use crate::bloom::dto::ViewDocument;
use crate::bloom::instructions::bundle;

/// The branch the day syncs back onto. Bloomery's mainline moves day to day;
/// what it returns to does not.
const MAIN: &str = "main";

/// The coordinator's own setting for the fleet repository, and the roll's
/// fallback when `--repo` names none.
const AUTHORITY_REPO: &str = "AETHER_BLOOMERY_AUTHORITY_REPO";

/// The coordinator's own setting for the instruction bundles it authorizes as
/// model-process policy (ADR-0214), which the hand-off compares the freshly
/// assembled bundle address against.
const AUTHORIZED_INSTRUCTIONS: &str = "AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS";

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
    /// Defaults to that variable; there is no compiled default.
    #[arg(long)]
    repo: Option<String>,

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
    roll_with_authorization(view, shell, coverage, args, configured_authorized_instructions().as_deref())
}

/// The roll pipeline with the operator's authorized-bundle knob supplied by the
/// caller, so tests drive each hand-off shape with an explicit value instead
/// of process environment.
fn roll_with_authorization(
    view: &ViewDocument,
    shell: &impl Shell,
    coverage: &DayCoverage,
    args: &RollArgs,
    authorized: Option<&str>,
) -> Result<String> {
    let from = sync_from(&args.from)?;
    let repo = Repo::new(authority_repo(args.repo.as_deref(), configured_authority_repo())?);
    preconditions::screen(view, shell, &repo, &args.date, &args.remote)?;
    let synced = sync::merge(shell, &repo, &args.remote, &from, coverage, args.replay)?;
    cut::create(shell, &repo, &args.remote, &args.date)?;
    // After the screen, never before: resolving the bundle reads the checkout
    // but writes nothing, and a refused roll must not have moved anything.
    Ok(handoff(&args.date, &synced, authorized, &bundle_address_hex()))
}

/// The fleet repository every roll `git` runs against, from `--repo` or the
/// coordinator's own setting.
///
/// There is no compiled default. Where the fleet repository sits is host
/// layout, which this repository neither knows nor should state, and the two
/// candidate defaults are both worse than a refusal: a baked-in path is one
/// host's directory shipped to every reader, and the process cwd silently
/// answers about whichever checkout the operator happened to be standing in
/// — the failure ADR-0203 rooted every call at the repository to end.
fn authority_repo(named: Option<&str>, configured: Option<String>) -> Result<String> {
    let named = named.map(str::trim).filter(|path| !path.is_empty()).map(ToOwned::to_owned);
    named.or(configured).ok_or_else(|| {
        anyhow!("the roll needs the fleet repository: pass --repo, or set {AUTHORITY_REPO} to the path the coordinator reads")
    })
}

/// `AETHER_BLOOMERY_AUTHORITY_REPO`, or nothing.
///
/// Operator tooling reading the coordinator's own repository setting, the way
/// this crate's port and control-token resolvers read theirs — not cap config,
/// which is what `clippy.toml` disallows the direct read to protect.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: xtask reads the coordinator's own repository setting; not cap config
fn configured_authority_repo() -> Option<String> {
    env::var(AUTHORITY_REPO).ok().map(|path| path.trim().to_owned()).filter(|path| !path.is_empty())
}

/// `AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS`, or nothing.
///
/// Operator tooling reading the coordinator's own authorized-bundle setting, the way
/// `configured_authority_repo` reads its repository setting — not cap config,
/// which is what `clippy.toml` disallows the direct read to protect.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: xtask reads the coordinator's own repository setting; not cap config
fn configured_authorized_instructions() -> Option<String> {
    env::var(AUTHORIZED_INSTRUCTIONS).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

/// The content address of the instruction bundle assembled from this checkout's
/// own instruction sources — the same [`bundle::imported`] and address the
/// `instructions` verb prints, so the hand-off compares what the operator would
/// record against what the coordinator authorizes.
fn bundle_address_hex() -> String {
    bundle::imported().address().to_hex()
}

/// Whether the operator's authorized-bundle knob already names `bundle_hex`.
///
/// The knob is a comma-separated list so a rotation can authorize the outgoing
/// and incoming bundles at once; naming the bundle anywhere in the list counts.
fn authorizes_bundle(authorized: &str, bundle_hex: &str) -> bool {
    authorized.split(',').map(str::trim).any(|entry| entry == bundle_hex)
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

/// The steps the command cannot perform, printed verbatim.
///
/// The mainline ref is boot configuration on the host, outside this repository
/// and outside this process, so the roll ends by handing the operator the exact
/// lines to set rather than editing an environment file it does not own.
///
/// The sewn batch can move the instruction-bundle address, so the hand-off
/// compares the freshly assembled bundle against the operator's
/// `AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS`: a knob naming another address
/// gains a third step — re-record the bundle and repoint before the restart,
/// or the deploy leaves every model dispatch refusing — while an unset knob
/// gains a one-line notice printing the address. A knob that already names the
/// bundle changes nothing.
fn handoff(day: &Day, synced: &str, authorized: Option<&str>, bundle_hex: &str) -> String {
    let authorized = authorized.map(str::trim).filter(|value| !value.is_empty());
    let stale = authorized.is_some_and(|value| !authorizes_bundle(value, bundle_hex));
    let steps = if stale {
        "three"
    } else {
        "two"
    };
    let mut handoff = format!(
        "synced the day onto main as {synced} and rolled onto {branch}.\n\
         \n\
         {steps} steps stay host-side, because the coordinator's mainline ref is boot configuration\n\
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
    );

    if stale {
        let _ = write!(
            handoff,
            "\n\
             \x20 3. re-record the moved instruction bundle and repoint its authorization before the restart:\n\
             \n\
             \x20      cargo xtask bloom instructions --record\n\
             \x20      AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS={bundle_hex}\n"
        );
    } else if authorized.is_none() {
        let _ = write!(
            handoff,
            "\n\
             notice: AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS is unset; the instruction bundle at {bundle_hex} stays \
             unauthorized until it is recorded and named there.\n"
        );
    }
    handoff
}

#[cfg(test)]
mod tests {
    use aether_bloomery::BloomStatus;

    use super::shell::Run;
    use super::shell::fake::Fake;
    use aether_bloomery_git::DayCoverage;

    use super::{
        AUTHORITY_REPO, AUTHORIZED_INSTRUCTIONS, Day, RollArgs, authority_repo, bundle_address_hex,
        roll_with_authorization, sync_from,
    };
    use crate::bloom::dto::{ViewDocument, test_bloom, test_member, test_view};
    use aether_bloomery::Digest;

    /// Roll a drained day with the operator's authorized-bundle knob reading as
    /// `authorized`.
    fn drained_roll(authorized: Option<&str>) -> String {
        roll_with_authorization(
            &drained_view(),
            &green(),
            &DayCoverage::green(),
            &args("bloomery/daily/2026-08-14"),
            authorized,
        )
        .expect("a drained day rolls")
    }

    /// A knob value that names any address but the bundle's: the real address
    /// with its first nibble flipped.
    fn other_address(bundle_hex: &str) -> String {
        let (first, rest) = bundle_hex.split_at(1);
        format!(
            "{}{rest}",
            if first == "0" {
                "1"
            } else {
                "0"
            }
        )
    }

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
            repo: Some("/srv/fleet.git".to_owned()),
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

        roll_with_authorization(
            &drained_view(),
            &shell,
            &DayCoverage::green(),
            &args("bloomery/daily/2026-08-14"),
            None,
        )
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
        let handoff = drained_roll(None);

        assert!(
            handoff.contains("AETHER_BLOOMERY_MAINLINE_REF=refs/heads/bloomery/daily/2026-08-15"),
            "the repoint line is pasteable: {handoff}"
        );
        assert!(handoff.contains("restart"), "the restart is named rather than skipped: {handoff}");
    }

    // A knob that already names this bundle changes nothing: the hand-off is
    // the two host-side steps, with no instruction-bundle aside. The knob is a
    // comma-separated list, so a rotation naming the bundle alongside the
    // outgoing one counts as naming it.
    #[test]
    fn the_handoff_stays_two_steps_when_the_knob_names_this_bundle() {
        let bundle_hex = bundle_address_hex();
        let rotated = format!("{},{}", other_address(&bundle_hex), bundle_hex);

        for authorized in [bundle_hex, rotated] {
            let handoff = drained_roll(Some(&authorized));

            assert!(handoff.contains("two steps stay host-side"), "still two steps: {handoff}");
            assert!(
                handoff.contains("AETHER_BLOOMERY_MAINLINE_REF=refs/heads/bloomery/daily/2026-08-15"),
                "the repoint line is pasteable: {handoff}"
            );
            assert!(
                !handoff.contains(AUTHORIZED_INSTRUCTIONS),
                "no bundle aside when the knob already names it: {handoff}"
            );
        }
    }

    // Tripwire: the sewn batch moves the bundle address, and a deploy that
    // follows a hand-off naming only the mainline repoint leaves the
    // authorization on the old address, refusing every model dispatch. A knob
    // naming another address gains the re-record as a third step, before the
    // restart.
    #[test]
    fn the_handoff_adds_a_record_step_when_the_knob_names_another_bundle() {
        let bundle_hex = bundle_address_hex();
        let handoff = drained_roll(Some(&other_address(&bundle_hex)));

        assert!(handoff.contains("three steps stay host-side"), "the third step is counted: {handoff}");
        assert!(handoff.contains("cargo xtask bloom instructions --record"), "the re-record is named: {handoff}");
        assert!(
            handoff.contains(&format!("{AUTHORIZED_INSTRUCTIONS}={bundle_hex}")),
            "the new address is printed pasteable: {handoff}"
        );
        assert!(handoff.contains("restart"), "the restart still follows the repoint: {handoff}");
    }

    // An unset knob cannot be stale, but the operator still needs the address
    // the sewn batch moved to: a one-line notice prints it.
    #[test]
    fn the_handoff_notices_an_unset_authorization_with_the_bundle_address() {
        let handoff = drained_roll(None);
        let bundle_hex = bundle_address_hex();

        assert!(handoff.contains("two steps stay host-side"), "still two steps: {handoff}");
        assert!(handoff.contains(AUTHORIZED_INSTRUCTIONS), "the knob is named even when unset: {handoff}");
        assert!(handoff.contains(&bundle_hex), "the bundle address is printed: {handoff}");
        assert!(
            handoff.lines().any(|line| line.contains(AUTHORIZED_INSTRUCTIONS) && line.contains(&bundle_hex)),
            "the notice is one line: {handoff}"
        );
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

        roll_with_authorization(&view, &shell, &DayCoverage::green(), &args("bloomery/daily/2026-08-14"), None)
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

        let refusal = roll_with_authorization(
            &drained_view(),
            &shell,
            &DayCoverage::hold("red test crate::day_head"),
            &args("bloomery/daily/2026-08-14"),
            None,
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

    // Tripwire: with neither the flag nor the setting, the roll refuses and
    // names what to set. A fallback here is the whole bug — the cwd or a baked
    // path both resolve to *a* repository, and a roll that advances main in the
    // wrong one is discovered a day later.
    #[test]
    fn an_unconfigured_fleet_repository_is_a_refusal_that_names_the_setting() {
        let refusal = authority_repo(None, None).expect_err("a roll with no repository configured refuses").to_string();

        assert!(refusal.contains("--repo"), "the flag is named: {refusal}");
        assert!(refusal.contains(AUTHORITY_REPO), "the setting is named: {refusal}");
        assert_eq!(authority_repo(Some("  /srv/fleet.git "), None).expect("the flag names it"), "/srv/fleet.git");
        assert_eq!(
            authority_repo(None, Some("/srv/fleet.git".to_owned())).expect("the setting names it"),
            "/srv/fleet.git"
        );
    }
}
