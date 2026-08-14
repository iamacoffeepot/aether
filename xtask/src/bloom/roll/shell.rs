//! The external programs the roll drives.
//!
//! `git` and `gh` behind one seam, following the precedent that xtask shells to
//! `git` rather than linking a git implementation. The seam is what lets the
//! roll's ordering — screen before mutate, fetch before cut, rebase never
//! squash — be exercised without a repository, a remote, or a GitHub session.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// What one captured invocation produced.
pub struct Run {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Shell {
    /// Run and capture, for anything the roll reads back.
    fn capture(&self, program: &str, args: &[&str]) -> Result<Run>;

    /// Run with the operator's terminal attached, for a wait whose whole value
    /// is watching it. `Ok(false)` is a clean non-zero exit.
    fn stream(&self, program: &str, args: &[&str]) -> Result<bool>;
}

/// The real programs on this host.
pub struct Host;

impl Shell for Host {
    fn capture(&self, program: &str, args: &[&str]) -> Result<Run> {
        let output =
            Command::new(program).args(args).output().with_context(|| format!("spawn {}", rendered(program, args)))?;
        Ok(Run {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn stream(&self, program: &str, args: &[&str]) -> Result<bool> {
        let status =
            Command::new(program).args(args).status().with_context(|| format!("spawn {}", rendered(program, args)))?;
        Ok(status.success())
    }
}

/// Run, and fail with the program's own diagnosis when it exits non-zero.
///
/// A roll step that fails has already said why — `gh` names the pull request it
/// could not open, `git` names the ref it could not resolve — so the failure is
/// forwarded rather than restated in this crate's words.
pub fn checked(shell: &impl Shell, program: &str, args: &[&str]) -> Result<String> {
    let run = shell.capture(program, args)?;
    if !run.success {
        let reason = if run.stderr.is_empty() {
            &run.stdout
        } else {
            &run.stderr
        };
        bail!("{} failed: {reason}", rendered(program, args));
    }
    Ok(run.stdout)
}

/// One invocation as an operator would have typed it.
pub fn rendered(program: &str, args: &[&str]) -> String {
    format!("{program} {}", args.join(" "))
}

#[cfg(test)]
pub(super) mod fake {
    use std::cell::RefCell;

    use anyhow::Result;

    use super::{Run, Shell, rendered};

    impl Run {
        /// A clean exit carrying `stdout`.
        pub(in crate::bloom::roll) fn ok(stdout: &str) -> Self {
            Self { success: true, stdout: stdout.to_owned(), stderr: String::new() }
        }

        /// A non-zero exit carrying `stderr`.
        pub(in crate::bloom::roll) fn failed(stderr: &str) -> Self {
            Self { success: false, stdout: String::new(), stderr: stderr.to_owned() }
        }
    }

    /// A shell that answers from a closure over the rendered command line and
    /// records every invocation in order — the ordering is half of what the
    /// roll's tests are about.
    pub(in crate::bloom::roll) struct Fake<'a> {
        reply: Box<dyn Fn(&str) -> Run + 'a>,
        calls: RefCell<Vec<String>>,
    }

    impl<'a> Fake<'a> {
        pub(in crate::bloom::roll) fn new(reply: impl Fn(&str) -> Run + 'a) -> Self {
            Self { reply: Box::new(reply), calls: RefCell::new(Vec::new()) }
        }

        pub(in crate::bloom::roll) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn answer(&self, program: &str, args: &[&str]) -> Run {
            let line = rendered(program, args);
            let reply = (self.reply)(&line);
            self.calls.borrow_mut().push(line);
            reply
        }
    }

    impl Shell for Fake<'_> {
        fn capture(&self, program: &str, args: &[&str]) -> Result<Run> {
            Ok(self.answer(program, args))
        }

        fn stream(&self, program: &str, args: &[&str]) -> Result<bool> {
            Ok(self.answer(program, args).success)
        }
    }
}
