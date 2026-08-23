//! The external programs the roll drives.
//!
//! `git` behind one seam, following the precedent that xtask shells to `git`
//! rather than linking a git implementation. The seam is what lets the roll's
//! ordering — screen before mutate, compare-and-swap before cut, replica push
//! after the advance — be exercised without a repository or a remote.

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

/// The fleet repository every roll `git` runs against (#5414).
///
/// The refs the roll reads and writes — the day branch, `refs/heads/main`, the
/// sync commit it builds over them — live in the fleet repository and nowhere
/// else. A call that inherits the process cwd therefore answers about whatever
/// checkout the operator happened to be standing in, which is how a roll driven
/// from a plain clone died on `unknown revision` with every ref it needed
/// sitting intact one directory away. Rooting each call at the repository makes
/// the answer a property of the repository named on the command line.
pub struct Repo(String);

impl Repo {
    /// Root the roll's git at `path` — the fleet repository, bare or a worktree
    /// of it.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// `git -C <repo> <args…>`, captured, failing with git's own words.
    ///
    /// # Errors
    /// The program could not be spawned, or it exited non-zero.
    pub fn checked(&self, shell: &impl Shell, args: &[&str]) -> Result<String> {
        checked(shell, "git", &self.rooted(args))
    }

    /// `git -C <repo> <args…>`, captured, with a non-zero exit left for the
    /// caller to read off [`Run::success`] — the shape a probe wants.
    ///
    /// # Errors
    /// The program could not be spawned.
    pub fn capture(&self, shell: &impl Shell, args: &[&str]) -> Result<Run> {
        shell.capture("git", &self.rooted(args))
    }

    fn rooted<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut rooted = Vec::with_capacity(args.len() + 2);
        rooted.push("-C");
        rooted.push(self.0.as_str());
        rooted.extend_from_slice(args);
        rooted
    }
}

/// Run, and fail with the program's own diagnosis when it exits non-zero.
///
/// A roll step that fails has already said why — `git` names the ref it could
/// not resolve or the compare-and-swap it lost — so the failure is forwarded
/// rather than restated in this crate's words.
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
