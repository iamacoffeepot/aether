//! Tomorrow's branch, cut from post-advance fleet main (ADR-0203).

use anyhow::Result;

use super::MAIN;
use super::day::Day;
use super::replica;
use super::shell::{Shell, checked};

/// Cut the day branch from the fleet main the sync-back just advanced, and
/// replicate it so the coordinator can be repointed at a ref that exists.
///
/// The replica push is best-effort on the same terms as the advance's: the
/// cut itself is the fleet ref, and a mirror that will not take it lags
/// rather than failing a day whose main has already moved.
pub fn create(shell: &impl Shell, remote: &str, day: &Day) -> Result<()> {
    let branch = day.branch();

    // Cut from `refs/heads/main` rather than a fetch of GitHub: the advance
    // just compare-and-swapped that ref in the fleet repository, and a GitHub
    // fetch would silently restore the pre-advance replica.
    checked(shell, "git", &["branch", "--no-track", &branch, &format!("refs/heads/{MAIN}")])?;

    println!("cut {branch} from post-advance {MAIN}");
    replica::push(shell, remote, &branch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::create;
    use crate::bloom::roll::day::Day;
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;

    // Tripwire: the cut is taken from the fleet main the sync-back just
    // advanced. Fetching GitHub first, or cutting from FETCH_HEAD, produces a
    // branch that looks right, pushes cleanly, and silently drops the day that
    // was just advanced — the failure has no symptom until someone reads the log.
    // Tripwire: a replica that rejects the push does not fail the cut. ADR-0203
    // makes GitHub an output of the roll, and the advance already treats its own
    // push as advisory; gating the cut on the same remote strands the estate with
    // main advanced and tomorrow uncut, which no operator can tell apart from a
    // roll that refused before it started.
    #[test]
    fn a_rejected_replica_push_does_not_fail_the_cut() {
        let shell = Fake::new(|line| {
            if line.starts_with("git push") {
                Run::failed("fatal: --mirror can't be combined with refspecs")
            } else {
                Run::ok("")
            }
        });
        let day = Day::parse("2026-08-15").expect("a well-formed day");

        create(&shell, "origin", &day).expect("a rejected replica does not fail the cut");

        assert_eq!(
            shell.calls(),
            [
                "git branch --no-track bloomery/daily/2026-08-15 refs/heads/main",
                "git push origin bloomery/daily/2026-08-15",
            ],
            "the cut is written to the fleet ref and the replica is still attempted"
        );
    }

    #[test]
    fn the_cut_is_taken_from_fleet_main() {
        let shell = Fake::new(|_| Run::ok(""));
        let day = Day::parse("2026-08-15").expect("a well-formed day");

        create(&shell, "origin", &day).expect("the cut succeeds");

        let calls = shell.calls();
        assert_eq!(
            calls,
            [
                "git branch --no-track bloomery/daily/2026-08-15 refs/heads/main",
                "git push origin bloomery/daily/2026-08-15",
            ],
            "the cut is taken from the fleet main the advance wrote, with no GitHub fetch"
        );
        assert!(
            !calls.iter().any(|line| line.contains("fetch") || line.contains("FETCH_HEAD")),
            "the cut does not fetch GitHub: {calls:?}"
        );
    }
}
