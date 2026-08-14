//! Tomorrow's branch, cut from post-sync main (ADR-0186).

use anyhow::Result;

use super::MAIN;
use super::day::Day;
use super::shell::{Shell, checked};

/// Cut the day branch from the main the sync-back just merged onto, and push
/// it so the coordinator can be repointed at a ref that exists.
pub fn create(shell: &impl Shell, remote: &str, day: &Day) -> Result<()> {
    let branch = day.branch();

    // Fetch first, and cut from `FETCH_HEAD` rather than a local `main` or a
    // remote-tracking ref: the cut has to be the main the sync-back produced
    // minutes ago, and `FETCH_HEAD` is by construction the commit this fetch
    // resolved. A local branch is whatever the operator's checkout last pulled,
    // which is a day of blooms short and fails silently — the branch is cut, it
    // just carries yesterday.
    checked(shell, "git", &["fetch", remote, MAIN])?;
    checked(shell, "git", &["branch", "--no-track", &branch, "FETCH_HEAD"])?;
    checked(shell, "git", &["push", remote, &branch])?;

    println!("cut {branch} from post-sync {remote}/{MAIN} and pushed it");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::create;
    use crate::bloom::roll::day::Day;
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;

    // Tripwire: the cut is taken from the main the sync-back just merged onto.
    // Cutting from a local ref that a fetch never refreshed produces a branch
    // that looks right, pushes cleanly, and silently drops the day that was just
    // synced back — the failure has no symptom until someone reads the log.
    #[test]
    fn the_cut_is_taken_from_freshly_fetched_main() {
        let shell = Fake::new(|_| Run::ok(""));
        let day = Day::parse("2026-08-15").expect("a well-formed day");

        create(&shell, "origin", &day).expect("the cut succeeds");

        let calls = shell.calls();
        assert_eq!(
            calls,
            [
                "git fetch origin main",
                "git branch --no-track bloomery/daily/2026-08-15 FETCH_HEAD",
                "git push origin bloomery/daily/2026-08-15",
            ],
            "the fetch precedes a cut taken from what it resolved, and the branch is pushed"
        );
    }
}
