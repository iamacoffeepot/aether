//! Grammar-recording stand-in for a model-lane harness binary.
//!
//! Each arm's smoke tests fork this instead of the real CLI. The stub records
//! argv and stdin and prints a canned transcript; the test asserts the grammar.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use super::TransformArgs;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// One recorded fork of the stand-in, in the order the arm launched it.
pub struct Launch {
    /// Arguments after the program name, one string per argv element.
    pub argv: Vec<String>,
    /// Bytes the child read from stdin. Empty when the arm passed `Stdio::null()`.
    pub stdin: Vec<u8>,
}

impl Launch {
    /// The value following `flag`, when it is present as a two-element window.
    pub fn flag(&self, flag: &str) -> Option<&str> {
        self.argv.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].as_str())
    }

    /// Whether `flag` appears as an argv element.
    pub fn has(&self, flag: &str) -> bool {
        self.argv.iter().any(|arg| arg == flag)
    }
}

/// A unique temp root holding an executable recorder and its launch log.
pub struct Stub {
    root: PathBuf,
    program: PathBuf,
    record: PathBuf,
}

#[derive(Clone, Copy)]
enum Mode {
    Succeed,
    FirstLaunchRefuses,
    FailAfterOutput,
}

impl Stub {
    /// A stub that prints a canned transcript and exits 0 on every launch.
    pub fn succeed() -> Self {
        Self::write(Mode::Succeed)
    }

    /// A stub whose first launch exits nonzero with empty stdout and a
    /// `No conversation found` stderr — the `resume_handle_rejected` shape.
    pub fn first_launch_refuses() -> Self {
        Self::write(Mode::FirstLaunchRefuses)
    }

    /// A stub that prints a canned transcript and then exits nonzero.
    pub fn fail_after_output() -> Self {
        Self::write(Mode::FailAfterOutput)
    }

    /// Absolute path of the stand-in executable, for `peak.command`.
    pub fn program(&self) -> &str {
        self.program.to_str().expect("stub path is utf-8")
    }

    /// Evidence directory this stub owns — unique, already created.
    pub fn out(&self) -> PathBuf {
        self.root.join("out")
    }

    /// Recorded launches in fork order.
    pub fn launches(&self) -> Vec<Launch> {
        let mut launches = Vec::new();
        let mut n = 1;
        loop {
            let dir = self.record.join(n.to_string());
            if !dir.is_dir() {
                break;
            }
            let argv = fs::read_to_string(dir.join("argv")).unwrap_or_default();
            let argv = if argv.is_empty() {
                Vec::new()
            } else {
                argv.lines().map(str::to_owned).collect()
            };
            launches.push(Launch { argv, stdin: fs::read(dir.join("stdin")).unwrap_or_default() });
            n += 1;
        }
        launches
    }

    fn write(mode: Mode) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-harness-stub-{}-{seq}", process::id()));
        let record = root.join("record");
        fs::create_dir_all(&record).expect("stub record dir");
        fs::create_dir_all(root.join("out")).expect("stub out dir");
        let program = root.join("stub");
        fs::write(&program, script(&record, mode)).expect("stub script");
        let mut permissions = fs::metadata(&program).expect("stub meta").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).expect("chmod stub");
        Self { root, program, record }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A `TransformArgs` for `command` writing evidence under `out`, with a unique
/// nonce so two tests sharing a process (and a host scratch root) do not reap
/// each other's `Scratch` directory.
pub fn args(command: impl Into<String>, out: PathBuf) -> TransformArgs {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    TransformArgs {
        command: command.into(),
        out,
        nonce: Some(format!("stub-{seq}")),
        subject: None,
        diff_base: None,
        harness: None,
        model: None,
        effort: None,
        task: None,
        resume: None,
        seeded: None,
        package: Vec::new(),
        partition: None,
        prepared: false,
    }
}

fn script(record: &Path, mode: Mode) -> String {
    let after_record = match mode {
        Mode::Succeed => "emit\n",
        Mode::FirstLaunchRefuses => {
            "if [ \"$n\" -eq 1 ]; then\n  echo 'No conversation found' >&2\n  exit 1\nfi\nemit\n"
        }
        Mode::FailAfterOutput => "emit\nexit 1\n",
    };
    format!(
        "#!/bin/sh\n\
         record='{}'\n\
         n=1\n\
         while [ -d \"$record/$n\" ]; do\n\
         \tn=$((n + 1))\n\
         done\n\
         mkdir -p \"$record/$n\"\n\
         for arg in \"$@\"; do\n\
         \tprintf '%s\\n' \"$arg\"\n\
         done > \"$record/$n/argv\"\n\
         cat > \"$record/$n/stdin\"\n",
        record.display(),
    ) + EMIT
        + after_record
}

const EMIT: &str = r#"
emit() {
  printf '%s\n' "{\"payload_type\":\"run.terminal.completed\",\"payload\":{\"kind\":\"run_terminal\",\"terminal\":\"completed\",\"text\":\"from-launch-$n\",\"reason\":null},\"stream\":{\"id\":\"aaaaaaaa-bbbb-8ccc-8ddd-eeeeeeeeeeee\"}}"
  printf '%s\n' "{\"type\":\"result\",\"is_error\":false,\"session_id\":\"stub-session-$n\",\"num_turns\":1,\"result\":\"from-launch-$n\"}"
}
"#;
