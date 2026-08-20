//! The GitHub replica push, which is an output of the roll and never a gate.
//!
//! ADR-0203 made the advance authoritative in the fleet repository and demoted
//! GitHub to a one-way replica. Both refs the roll writes — the advanced main
//! and tomorrow's cut — replicate on the same terms: an unreachable or
//! rejecting remote is reported and the roll carries on, because a replica that
//! cannot be written is a stale mirror rather than a failed day. Gating either
//! push on the remote strands the estate with main advanced and tomorrow uncut,
//! which is the one outcome the roll's ordering exists to prevent.

use super::shell::Shell;

/// Push one ref to the replica, best-effort, naming whatever did not replicate.
pub fn push(shell: &impl Shell, remote: &str, refname: &str) {
    let run = match shell.capture("git", &["push", remote, refname]) {
        Ok(run) => run,
        Err(error) => return report(refname, &error.to_string()),
    };

    if run.success {
        println!("replicated {refname} to {remote}");
        return;
    }

    report(
        refname,
        if run.stderr.is_empty() {
            &run.stdout
        } else {
            &run.stderr
        },
    );
}

fn report(refname: &str, reason: &str) {
    println!("best-effort replica push of {refname} failed, so the mirror lags: {reason}");
}
