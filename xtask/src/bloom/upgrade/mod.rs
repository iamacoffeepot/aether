//! `cargo xtask bloom upgrade` — fold-test a candidate before replacing the coordinator.
//!
//! Replacing the coordinator binary over the live journal is the highest-stakes
//! routine procedure the system has. The two known kill-shapes are a wire
//! reshape that fatal-aborts boot replay (#4942) and a store copy taken
//! without its WAL sidecars, which replays a stale prefix silently. This
//! command is the refusing tool for that sequence: screen, fold-test on a
//! copy, only then install and restart through the supervisor.

mod deploy;
mod fold;
mod paths;
mod preconditions;
mod shell;

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use self::paths::Paths;
use self::shell::Host;
use crate::bloom::Endpoint;
use crate::bloom::client::Client;
use crate::bloom::dto::ViewDocument;

/// Fold-test a candidate coordinator and replace the running binary if it holds.
#[derive(Args, Debug)]
pub struct UpgradeArgs {
    /// The candidate coordinator binary to fold-test and, if it holds, install.
    #[arg(long)]
    candidate: PathBuf,

    /// The running coordinator binary the supervisor launches.
    #[arg(long)]
    bin: PathBuf,

    /// The live journal database (the `-wal`/`-shm` sidecars sit beside it).
    #[arg(long)]
    store: PathBuf,

    /// The systemd user unit that supervises the coordinator.
    #[arg(long, default_value = "bloomery")]
    unit: String,

    /// Directory for the fold-test store copy and the candidate's scratch dirs.
    #[arg(long)]
    scratch_dir: Option<PathBuf>,

    /// REST port the fold-test candidate binds.
    #[arg(long, default_value_t = 18910)]
    fold_http_port: u16,

    /// RPC port the fold-test candidate binds.
    #[arg(long, default_value_t = 18909)]
    fold_rpc_port: u16,

    /// How long to wait for the fold-test candidate to serve `/view`.
    #[arg(long, default_value_t = 30_000)]
    fold_timeout_millis: u64,

    /// How often to poll the fold-test `/view`.
    #[arg(long, default_value_t = 200)]
    fold_poll_millis: u64,

    /// How long to wait after restart for `/view` observation to come back.
    #[arg(long, default_value_t = 30_000)]
    observe_timeout_millis: u64,

    /// How often to poll `/view` while waiting for observation.
    #[arg(long, default_value_t = 200)]
    observe_poll_millis: u64,

    /// Path template for the running process's executable, `$pid` replaced.
    #[arg(long, default_value = "/proc/$pid/exe")]
    proc_exe: String,

    /// Suffix for the backup taken beside the running binary.
    #[arg(long, default_value = ".prev")]
    backup_suffix: String,
}

/// `/view` as the upgrade reads it: the live coordinator, and the fold-test
/// candidate once it has booted against the copy.
pub(super) trait Views {
    fn live(&self) -> Result<ViewDocument>;
    fn folded(&self) -> Result<ViewDocument>;
}

struct RestViews<'a> {
    live: &'a Client<'a>,
    fold: Endpoint,
}

impl Views for RestViews<'_> {
    fn live(&self) -> Result<ViewDocument> {
        self.live.view()
    }

    fn folded(&self) -> Result<ViewDocument> {
        Client::new(&self.fold).view()
    }
}

/// Drive one upgrade against the live coordinator.
pub fn run(client: &Client<'_>, args: &UpgradeArgs) -> Result<String> {
    let fold = Endpoint { host: "127.0.0.1".to_owned(), port: args.fold_http_port, token: None };
    let views = RestViews { live: client, fold };
    upgrade(&client.view()?, &views, &Host, args)
}

fn upgrade(view: &ViewDocument, views: &impl Views, shell: &impl shell::Shell, args: &UpgradeArgs) -> Result<String> {
    let paths = Paths::resolve(args)?;
    let mut log = String::new();
    preconditions::screen(view, shell, &paths, &mut log)?;
    fold::prove(views, shell, &paths, &mut log)?;
    deploy::apply(views, shell, &paths, &views.live()?, &mut log)?;
    Ok(log)
}

fn checked(log: &mut String, message: &str) {
    let _ = writeln!(log, "checked: {message}");
}

#[cfg(test)]
mod tests_support {
    use std::cell::RefCell;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Result, anyhow};

    use super::Views;
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use crate::bloom::upgrade::paths::Paths;
    use aether_bloomery::BloomStatus;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    pub fn unique_temp(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!("{prefix}-{}-{}", process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)))
    }

    /// Point `proc_exe` at a scratch `/proc/$pid/exe` whose environ carries
    /// only the named `PATH` plus an unrelated entry the reader must ignore.
    pub fn install_service_environ(proc_exe: &mut String, pid: &str, path: &str) {
        let root = unique_temp("aether-xtask-upgrade-proc");
        *proc_exe = format!("{}/$pid/exe", root.display());
        let dir = root.join(pid);
        fs::create_dir_all(&dir).expect("service process dir");
        fs::write(dir.join("environ"), format!("UNRELATED=secret\0PATH={path}\0")).expect("service process environ");
    }

    pub fn drained_view() -> ViewDocument {
        ViewDocument {
            mainline: DigestHex::from_bytes([1; 32]),
            observed: DigestHex::from_bytes([2; 32]),
            blooms: vec![BloomView {
                id: DigestHex::from_bytes([3; 32]),
                status: BloomStatus::Landed,
                superseded_by: None,
                members: vec![MemberView {
                    workpiece: "issue-5014".to_owned(),
                    scope_revision: DigestHex::from_bytes([7; 32]),
                    awaiting_surface: None,
                    withdrawn: None,
                }],
            }],
        }
    }

    pub fn test_paths() -> Paths {
        Paths {
            candidate: PathBuf::from("/opt/candidate"),
            bin: PathBuf::from("/opt/bloomery"),
            store: PathBuf::from("/var/bloomery.db"),
            scratch: PathBuf::from("/tmp/bloomery-upgrade-scratch"),
            unit: "bloomery".to_owned(),
            fold_http_port: 18910,
            fold_rpc_port: 18909,
            fold_timeout_millis: 0,
            fold_poll_millis: 0,
            observe_timeout_millis: 0,
            observe_poll_millis: 0,
            proc_exe: "/proc/$pid/exe".to_owned(),
            backup_suffix: ".prev".to_owned(),
        }
    }

    pub fn test_args() -> super::UpgradeArgs {
        super::UpgradeArgs {
            candidate: PathBuf::from("/opt/candidate"),
            bin: PathBuf::from("/opt/bloomery"),
            store: PathBuf::from("/var/bloomery.db"),
            unit: "bloomery".to_owned(),
            scratch_dir: Some(PathBuf::from("/tmp/bloomery-upgrade-scratch")),
            fold_http_port: 18910,
            fold_rpc_port: 18909,
            fold_timeout_millis: 0,
            fold_poll_millis: 0,
            observe_timeout_millis: 0,
            observe_poll_millis: 0,
            proc_exe: "/proc/$pid/exe".to_owned(),
            backup_suffix: ".prev".to_owned(),
        }
    }

    pub fn write_journal(path: &Path, rows: u64) {
        let connection = rusqlite::Connection::open(path).expect("open journal fixture");
        connection.execute("CREATE TABLE journal (n INTEGER NOT NULL)", []).expect("create journal table");

        let mut insert = connection.prepare("INSERT INTO journal (n) VALUES (?1)").expect("prepare insert");
        for n in 0..rows {
            insert.execute([n]).expect("insert journal row");
        }
    }

    pub fn seeded_paths(live_rows: u64, copy_rows: u64) -> Paths {
        let root = unique_temp("aether-xtask-upgrade-fold");
        fs::create_dir_all(&root).expect("create fold fixture root");
        let store = root.join("live.db");
        write_journal(&store, live_rows);
        let scratch = root.join("scratch");
        fs::create_dir_all(&scratch).expect("create fold scratch");
        write_journal(&scratch.join("fold.db"), copy_rows);
        let mut paths = test_paths();
        paths.store = store;
        paths.scratch = scratch;
        paths
    }

    pub fn seeded_args(live_rows: u64, copy_rows: u64) -> super::UpgradeArgs {
        let paths = seeded_paths(live_rows, copy_rows);
        let mut args = test_args();
        args.store = paths.store;
        args.scratch_dir = Some(paths.scratch);
        args
    }

    pub struct Scripted {
        live: Result<ViewDocument, String>,
        folded: Result<ViewDocument, String>,
        live_next: RefCell<Vec<Result<ViewDocument, String>>>,
        folded_next: RefCell<Vec<Result<ViewDocument, String>>>,
    }

    impl Scripted {
        pub fn new(live: Result<ViewDocument, String>, folded: Result<ViewDocument, String>) -> Self {
            Self { live, folded, live_next: RefCell::new(Vec::new()), folded_next: RefCell::new(Vec::new()) }
        }

        pub fn matching() -> Self {
            Self::new(Ok(drained_view()), Ok(drained_view()))
        }

        pub fn first_live(self, view: ViewDocument) -> Self {
            self.live_next.borrow_mut().push(Ok(view));
            self
        }

        pub fn first_folded(self, view: ViewDocument) -> Self {
            self.folded_next.borrow_mut().push(Ok(view));
            self
        }

        fn take(
            queue: &RefCell<Vec<Result<ViewDocument, String>>>,
            fallback: &Result<ViewDocument, String>,
        ) -> Result<ViewDocument> {
            queue.borrow_mut().pop().unwrap_or_else(|| fallback.clone()).map_err(|error| anyhow!("{error}"))
        }
    }

    impl Views for Scripted {
        fn live(&self) -> Result<ViewDocument> {
            Self::take(&self.live_next, &self.live)
        }

        fn folded(&self) -> Result<ViewDocument> {
            Self::take(&self.folded_next, &self.folded)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::shell::Run;
    use super::shell::fake::Fake;
    use super::tests_support::{Scripted, drained_view, seeded_args, test_args};
    use super::upgrade;
    use crate::bloom::dto::DigestHex;
    use aether_bloomery::BloomStatus;

    fn present_candidate(mut args: super::UpgradeArgs) -> super::UpgradeArgs {
        let path = super::tests_support::unique_temp("aether-xtask-upgrade-candidate");
        fs::write(&path, b"candidate").expect("write a present candidate");
        args.candidate = path;
        super::tests_support::install_service_environ(&mut args.proc_exe, "4321", "/service/bin");
        args
    }

    fn green() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("ActiveState") => Run::ok("active"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.starts_with("readlink") && line.contains("/proc/") => Run::ok("/opt/bloomery"),
            line if line.starts_with("readlink") => Run::ok("/opt/bloomery"),
            line if line.starts_with("test -e") => Run::ok(""),
            _ => Run::ok(""),
        })
    }

    fn matching() -> Scripted {
        Scripted::matching()
    }

    #[test]
    fn the_happy_path_prints_each_verification() {
        let args = present_candidate(seeded_args(12, 12));
        let shell = green();
        let log = upgrade(&drained_view(), &matching(), &shell, &args).expect("a drained upgrade holds");

        assert!(log.contains("checked: no undrained blooms"), "the screen is printed: {log}");
        assert!(log.contains("exists"), "the candidate is printed: {log}");
        assert!(log.contains("reachable"), "the unit is printed: {log}");
        assert!(log.contains("candidate doctor passed on service PATH"), "the doctor is a named check: {log}");
        assert!(log.contains("journal rows live=12 copy=12"), "row counts are printed: {log}");
        assert!(
            !shell.calls().iter().any(|line| line.starts_with("sqlite3")),
            "the upgrade path does not shell out to sqlite3: {:?}",
            shell.calls()
        );
        assert!(log.contains("matches live"), "the fold-test match is printed: {log}");
        assert!(log.contains("lanes disabled"), "lanes-off is printed: {log}");
        assert!(log.contains("backed up"), "the backup is printed: {log}");
        assert!(log.contains("installed candidate"), "the install is printed: {log}");
        assert!(log.contains("through the supervisor"), "the restart is printed: {log}");
        assert!(log.contains("process executable is /opt/bloomery"), "identity is printed: {log}");
        assert!(log.contains("observation advanced"), "observation is printed: {log}");
        let _ = fs::remove_file(&args.candidate);
    }

    // Tripwire: any refusal — undrained, short copy, reshape — has to leave
    // the live unit untouched. A restart recorded on a refused run is how a
    // fold-test that would have saved the journal is skipped because the
    // process is already the candidate.
    #[test]
    fn a_refused_upgrade_does_not_restart_the_live_coordinator() {
        let mut view = drained_view();
        view.blooms[0].status = BloomStatus::Sealed;
        let args = present_candidate(test_args());
        let shell = green();

        upgrade(&view, &matching(), &shell, &args).expect_err("an undrained upgrade is refused");

        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.contains("restart")), "the live unit is not restarted: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.starts_with("sqlite3") || line.starts_with("cp ")),
            "the store is not copied and the binary is not replaced: {calls:?}"
        );
        let _ = fs::remove_file(&args.candidate);
    }

    // Tripwire: a red doctor is a precondition refusal. Copying the store,
    // replacing the binary, or restarting after that report is how a host
    // missing jscpd becomes the live coordinator.
    #[test]
    fn a_red_doctor_does_not_copy_install_or_restart() {
        let args = present_candidate(test_args());
        let shell = Fake::new(|line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.contains("--doctor") => Run::failed("jscpd            MISSING  npm install -g jscpd"),
            _ => Run::ok(""),
        });

        let refusal =
            upgrade(&drained_view(), &matching(), &shell, &args).expect_err("a red doctor is refused").to_string();

        assert!(refusal.contains("jscpd"), "the doctor output is surfaced: {refusal}");
        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.contains("restart")), "the live unit is not restarted: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.starts_with("sqlite3") || line.starts_with("cp ")),
            "the store is not copied and the binary is not replaced: {calls:?}"
        );
        let _ = fs::remove_file(&args.candidate);
    }

    #[test]
    fn a_reshape_refusal_does_not_restart() {
        let decode = "record 2 decision did not decode: aether wire: invalid bool/presence byte 11";
        let args = present_candidate(seeded_args(12, 12));
        let shell = Fake::new(move |line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.contains("--store-path") => Run::failed(decode),
            line if line.starts_with("test -e") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal =
            upgrade(&drained_view(), &matching(), &shell, &args).expect_err("a reshape is refused").to_string();

        assert!(refusal.contains(decode), "the decode error is surfaced: {refusal}");
        assert!(
            !shell.calls().iter().any(|line| line.contains("restart")),
            "the live unit is not restarted: {:?}",
            shell.calls()
        );
        let _ = fs::remove_file(&args.candidate);
    }

    // Tripwire: the fold-test runs long enough for the live coordinator to
    // admit an observation. Waiting for the pre-fold observed digest then
    // times out a successful restart — the new process folded the later
    // journal and will never serve the stale one.
    #[test]
    fn a_live_observation_during_the_fold_test_is_the_deploy_baseline() {
        let mut later = drained_view();
        later.observed = DigestHex::from_bytes([4; 32]);
        let expected = format!("observed={}", later.observed);
        let views = Scripted::new(Ok(later), Ok(drained_view()));
        let args = present_candidate(seeded_args(12, 12));

        let log = upgrade(&drained_view(), &views, &green(), &args)
            .expect("the later observation is what the restart wait demands");

        assert!(log.contains(&expected), "deploy waits for the post-fold live digest: {log}");
        let _ = fs::remove_file(&args.candidate);
    }
}
