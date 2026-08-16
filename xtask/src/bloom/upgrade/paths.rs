//! Host paths and ports the upgrade reads from flags, not from literals.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str;

use anyhow::{Context, Result, bail};

use super::UpgradeArgs;

/// Resolved host locations the rest of the upgrade talks in.
pub struct Paths {
    pub candidate: PathBuf,
    pub bin: PathBuf,
    pub store: PathBuf,
    pub scratch: PathBuf,
    pub unit: String,
    pub fold_http_port: u16,
    pub fold_rpc_port: u16,
    pub fold_timeout_millis: u64,
    pub fold_poll_millis: u64,
    pub observe_timeout_millis: u64,
    pub observe_poll_millis: u64,
    pub proc_exe: String,
    pub backup_suffix: String,
}

impl Paths {
    pub fn resolve(args: &UpgradeArgs) -> Result<Self> {
        if args.store.as_os_str() == ":memory:" {
            bail!("refusing to upgrade: store path is :memory: (not a durable journal that can be fold-tested)");
        }
        let scratch = args.scratch_dir.clone().unwrap_or_else(|| env::temp_dir().join("bloomery-upgrade-fold"));
        Ok(Self {
            candidate: args.candidate.clone(),
            bin: args.bin.clone(),
            store: args.store.clone(),
            scratch,
            unit: args.unit.clone(),
            fold_http_port: args.fold_http_port,
            fold_rpc_port: args.fold_rpc_port,
            fold_timeout_millis: args.fold_timeout_millis,
            fold_poll_millis: args.fold_poll_millis,
            observe_timeout_millis: args.observe_timeout_millis,
            observe_poll_millis: args.observe_poll_millis,
            proc_exe: args.proc_exe.clone(),
            backup_suffix: args.backup_suffix.clone(),
        })
    }

    /// The live unit process's environment file, derived from the configured
    /// `/proc/$pid/exe` template so a remounted proc still names that process.
    ///
    /// `$pid` is replaced and the last component becomes `environ`. A pid that
    /// is not the unit's `MainPID` is refused rather than opening another
    /// process's environment.
    pub fn proc_environ(&self, pid: &str) -> Result<PathBuf> {
        let pid = unit_pid(pid)?;
        Ok(PathBuf::from(self.proc_exe.replace("$pid", pid)).with_file_name("environ"))
    }

    /// The live unit process's `PATH`, and only that entry.
    ///
    /// Other environ values stay in the file. A missing process-state file or a
    /// non-UTF-8 `PATH` is a refusal in the same style as a non-UTF-8 host path.
    pub fn process_path(&self, pid: &str) -> Result<String> {
        let environ = self.proc_environ(pid)?;
        let bytes =
            fs::read(&environ).with_context(|| format!("process environment {} is missing", environ.display()))?;
        path_from_environ(&bytes, &environ)
    }
}

/// A `MainPID` the upgrade may open: digits, not empty, not `0`.
fn unit_pid(pid: &str) -> Result<&str> {
    let pid = pid.trim();
    if pid.is_empty() || pid == "0" || !pid.bytes().all(|b| b.is_ascii_digit()) {
        bail!("supervisor MainPID is missing");
    }
    Ok(pid)
}

fn path_from_environ(bytes: &[u8], source: &Path) -> Result<String> {
    for entry in bytes.split(|byte| *byte == 0) {
        if let Some(value) = entry.strip_prefix(b"PATH=") {
            let path = str::from_utf8(value).with_context(|| format!("{} PATH is not UTF-8", source.display()))?;
            return Ok(path.to_owned());
        }
    }
    bail!("process environment {} has no PATH", source.display())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::super::tests_support::{test_args, test_paths, unique_temp};
    use super::Paths;

    // Tripwire: `:memory:` is the compiled store default. Copying it, or
    // folding against it, is not a journal — it is an empty private database
    // that cannot catch the reshape the fold-test exists to refuse.
    #[test]
    fn a_memory_store_is_refused_before_anything_is_copied() {
        let mut args = test_args();
        args.store = PathBuf::from(":memory:");
        let Err(error) = Paths::resolve(&args) else {
            panic!("an in-memory store is not fold-testable");
        };
        let refusal = error.to_string();
        assert!(refusal.contains(":memory:"), "the refusal names the sentinel: {refusal}");
    }

    fn paths_with_environ(pid: &str, bytes: &[u8]) -> Paths {
        let mut paths = test_paths();
        let root = unique_temp("aether-xtask-upgrade-environ");
        paths.proc_exe = format!("{}/$pid/exe", root.display());
        let dir = root.join(pid);
        fs::create_dir_all(&dir).expect("process dir");
        fs::write(dir.join("environ"), bytes).expect("process environ");
        paths
    }

    // Tripwire: the doctor needs the unit process's PATH, not the operator
    // shell's. Deriving environ from the exe template is what keeps a
    // remounted proc on that process; opening another pid would leak or
    // miss the service kit.
    #[test]
    fn process_environ_is_the_exe_template_sibling() {
        let mut paths = test_paths();
        paths.proc_exe = "/host/proc/$pid/exe".to_owned();
        let environ = paths.proc_environ("4321").expect("a live pid derives environ");
        assert_eq!(environ, PathBuf::from("/host/proc/4321/environ"));
    }

    // Tripwire: MainPID 0 is "no process" in systemd. Substituting it, or
    // walking to pid 1, would read a different environment than the unit.
    #[test]
    fn a_missing_main_pid_does_not_open_another_process_environment() {
        let paths = test_paths();
        for pid in ["", "0", "../1", "self"] {
            let Err(error) = paths.proc_environ(pid) else {
                panic!("{pid:?} must not derive an environ path");
            };
            let refusal = error.to_string();
            assert!(
                refusal.contains("MainPID") && refusal.contains("missing"),
                "a non-unit pid is missing process state: {refusal}"
            );
        }
    }

    // Tripwire: the file is NUL-delimited KEY=value. Returning the whole
    // blob, or the first entry, would forward credentials the doctor does
    // not need and would miss PATH when it is not first.
    #[test]
    fn only_path_is_taken_from_the_process_environment() {
        let paths = paths_with_environ("9", b"SECRET=hunter2\0PATH=/service/bin\0HOME=/var/lib/bloomery\0");
        let path = paths.process_path("9").expect("PATH is present");
        assert_eq!(path, "/service/bin");
        assert!(!path.contains("hunter2"), "unrelated values stay in the file");
    }

    #[test]
    fn a_missing_process_environment_is_refused() {
        let mut paths = test_paths();
        paths.proc_exe = format!("{}/$pid/exe", unique_temp("aether-xtask-upgrade-missing-proc").display());
        let Err(error) = paths.process_path("9") else {
            panic!("a missing environ file is process state that is not there");
        };
        let refusal = error.to_string();
        assert!(refusal.contains("missing"), "the refusal names the missing state: {refusal}");
        assert!(refusal.contains("environ"), "and names the file it wanted: {refusal}");
    }

    #[test]
    fn a_non_utf8_service_path_is_refused() {
        let paths = paths_with_environ("9", b"PATH=\xff\xff\0");
        let Err(error) = paths.process_path("9") else {
            panic!("a non-UTF-8 PATH is not a service path");
        };
        let refusal = error.to_string();
        assert!(refusal.contains("is not UTF-8"), "the refusal matches host-path style: {refusal}");
    }

    #[test]
    fn a_process_environment_without_path_is_refused() {
        let paths = paths_with_environ("9", b"HOME=/var/lib/bloomery\0");
        let Err(error) = paths.process_path("9") else {
            panic!("no PATH is not a service path");
        };
        let refusal = error.to_string();
        assert!(refusal.contains("has no PATH"), "the refusal names the missing entry: {refusal}");
    }
}
