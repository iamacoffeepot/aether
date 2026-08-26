//! Fold-test the candidate against a copy of the live journal.
//!
//! The copy is taken *with* the `-wal`/`-shm` sidecars. A store file copied
//! alone is a stale prefix of the same mainline digest — the #4942-adjacent
//! kill-shape that looks healthy and has fewer rows. Row counts are compared
//! before the candidate boots; a mismatch is a refusal that names both. The
//! candidate then boots against the copy on scratch ports, lanes disabled, so
//! a replay-breaking reshape dies here with its decode error and the live
//! process is never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use super::paths::Paths;
use super::shell::{Shell, checked};
use super::{Views, checked as note};
use crate::bloom::dto::ViewDocument;

/// Sidecars a WAL-mode database keeps beside the main file. Omitting either is
/// how a copy silently loses the tail of the journal.
const SIDECARS: [&str; 2] = ["-wal", "-shm"];

/// Prove the candidate can fold the live journal, then stop it.
pub fn prove(views: &impl Views, shell: &impl Shell, paths: &Paths, log: &mut String) -> Result<ViewDocument> {
    let copy = copy_store(shell, paths, log)?;
    compare_row_counts(&paths.store, &copy, log)?;
    let folded = boot_and_read(views, shell, paths, &copy, log)?;
    note(log, &format!("fold-test mainline={} blooms={} matches live", folded.mainline, folded.blooms.len()));
    Ok(folded)
}

fn copy_store(shell: &impl Shell, paths: &Paths, log: &mut String) -> Result<PathBuf> {
    fs::create_dir_all(&paths.scratch)
        .with_context(|| format!("create fold-test scratch {}", paths.scratch.display()))?;
    let copy = paths.scratch.join("fold.db");
    let from = path_arg(&paths.store)?;
    let to = path_arg(&copy)?;
    checked(shell, "cp", &[from, to])?;

    let mut copied = vec!["db".to_owned()];
    for suffix in SIDECARS {
        let sidecar = sidecar_path(&paths.store, suffix);
        if exists(shell, &sidecar)? {
            let dest = sidecar_path(&copy, suffix);
            checked(shell, "cp", &[path_arg(&sidecar)?, path_arg(&dest)?])?;
            copied.push(suffix.trim_start_matches('-').to_owned());
        }
    }

    note(log, &format!("copied {} to {} with sidecars {}", paths.store.display(), copy.display(), copied.join(" ")));
    Ok(copy)
}

fn compare_row_counts(live: &Path, copy: &Path, log: &mut String) -> Result<()> {
    let live_rows = journal_rows(live)?;
    let copy_rows = journal_rows(copy)?;
    if live_rows != copy_rows {
        bail!(
            "refusing to upgrade: store copy journal row count diverged from live\n  \
             live={live_rows} copy={copy_rows}"
        );
    }
    note(log, &format!("journal rows live={live_rows} copy={copy_rows}"));
    Ok(())
}

fn journal_rows(store: &Path) -> Result<u64> {
    let connection = Connection::open_with_flags(store, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open journal store {}", store.display()))?;
    let mut statement = connection
        .prepare("SELECT COUNT(*) FROM journal")
        .with_context(|| format!("prepare journal count for {}", store.display()))?;

    let count = statement
        .query_row([], |row| row.get::<_, i64>(0))
        .with_context(|| format!("query journal row count from {}", store.display()))?;
    u64::try_from(count).with_context(|| format!("journal row count from {} is not a number: {count}", store.display()))
}

fn boot_and_read(
    views: &impl Views,
    shell: &impl Shell,
    paths: &Paths,
    copy: &Path,
    log: &mut String,
) -> Result<ViewDocument> {
    let artifacts = paths.scratch.join("artifacts");
    let worktrees = paths.scratch.join("worktrees");
    fs::create_dir_all(&artifacts).with_context(|| format!("create {}", artifacts.display()))?;
    fs::create_dir_all(&worktrees).with_context(|| format!("create {}", worktrees.display()))?;

    let http = paths.fold_http_port.to_string();
    let rpc = paths.fold_rpc_port.to_string();
    let copy_s = path_arg(copy)?.to_owned();
    let artifacts_s = path_arg(&artifacts)?.to_owned();
    let worktrees_s = path_arg(&worktrees)?.to_owned();
    let candidate = path_arg(&paths.candidate)?;

    let args = [
        "--store-path",
        &copy_s,
        "--http-port",
        &http,
        "--rpc-port",
        &rpc,
        "--github-local-lane-enabled=false",
        "--artifacts-root",
        &artifacts_s,
        "--github-local-worktree-base",
        &worktrees_s,
    ];
    let env = [
        ("AETHER_STORE_PATH", copy_s.as_str()),
        ("AETHER_HTTP_PORT", http.as_str()),
        ("AETHER_RPC_PORT", rpc.as_str()),
        ("AETHER_GITHUB_LOCAL_LANE_ENABLED", "false"),
        ("AETHER_ARTIFACTS_ROOT", artifacts_s.as_str()),
        ("GITHUB_TOKEN", ""),
    ];

    let stderr_log = paths.scratch.join("fold.stderr");
    let mut session = shell.launch(candidate, &args, &env, &stderr_log)?;
    let outcome = poll_fold(views, shell, paths, session.as_mut());
    let _ = session.terminate();
    let folded = outcome?;
    note(log, "fold-test candidate booted against the copy with lanes disabled");
    Ok(folded)
}

fn poll_fold(
    views: &impl Views,
    shell: &impl Shell,
    paths: &Paths,
    session: &mut dyn super::shell::Session,
) -> Result<ViewDocument> {
    let deadline = Instant::now() + Duration::from_millis(paths.fold_timeout_millis);
    let mut last_pair = None;
    let mut last_error = None;
    loop {
        if let Some(exit) = session.try_wait()? {
            let detail = if exit.stderr.is_empty() {
                exit.stdout
            } else {
                exit.stderr
            };
            bail!("refusing to upgrade: fold-test aborted before serving /view\n  {detail}");
        }

        match (views.folded(), views.live()) {
            (Ok(folded), Ok(live)) if same_fold(&live, &folded) => return Ok(folded),
            (Ok(folded), Ok(live)) => last_pair = Some((live, folded)),
            (Err(error), _) | (_, Err(error)) => last_error = Some(error.to_string()),
        }

        if Instant::now() >= deadline {
            match last_pair {
                Some((live, folded)) => bail!(diverged_refusal(&live, &folded)),
                None => bail!(
                    "refusing to upgrade: fold-test did not serve /view within {} millis (not yet)\n  {}",
                    paths.fold_timeout_millis,
                    last_error.unwrap_or_else(|| "no /view yet".to_owned()),
                ),
            }
        }
        shell.pause(paths.fold_poll_millis);
    }
}

fn same_fold(live: &ViewDocument, folded: &ViewDocument) -> bool {
    live.mainline == folded.mainline && live.blooms.len() == folded.blooms.len()
}

fn diverged_refusal(live: &ViewDocument, folded: &ViewDocument) -> String {
    format!(
        "refusing to upgrade: fold-test diverged from the live coordinator\n  \
         live    mainline={} blooms={}\n  \
         folded  mainline={} blooms={}",
        live.mainline,
        live.blooms.len(),
        folded.mainline,
        folded.blooms.len(),
    )
}

fn exists(shell: &impl Shell, path: &Path) -> Result<bool> {
    Ok(shell.capture("test", &["-e", path_arg(path)?])?.success)
}

fn sidecar_path(store: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", store.display()))
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{SIDECARS, prove, sidecar_path};
    use crate::bloom::upgrade::shell::Run;
    use crate::bloom::upgrade::shell::fake::Fake;
    use crate::bloom::upgrade::tests_support::{Scripted, drained_view, seeded_paths};
    use aether_bloomery::Digest;

    fn matching() -> Scripted {
        Scripted::matching()
    }

    fn folding() -> impl Fn(&str) -> Run {
        |line| match line {
            line if line.starts_with("test -e") => Run::ok(""),
            _ => Run::ok(""),
        }
    }

    fn no_sqlite3(shell: &Fake<'_>) {
        assert!(
            !shell.calls().iter().any(|line| line.starts_with("sqlite3")),
            "the upgrade path does not shell out to sqlite3: {:?}",
            shell.calls()
        );
    }

    // Tripwire: a store file copied without its WAL is the same mainline
    // digest with fewer rows and no error — the copy looks healthy and the
    // upgrade proceeds onto a prefix. Naming both counts is the only signal.
    #[test]
    fn a_store_copy_with_divergent_row_counts_is_refused_naming_both() {
        let shell = Fake::new(folding());
        let mut log = String::new();

        let refusal = prove(&matching(), &shell, &seeded_paths(156, 140), &mut log)
            .expect_err("a short copy is refused")
            .to_string();

        assert!(refusal.contains("live=156"), "the live count is named: {refusal}");
        assert!(refusal.contains("copy=140"), "the copy count is named: {refusal}");
        assert!(
            !shell.calls().iter().any(|line| line.contains("--store-path")),
            "the candidate is not launched after a short copy: {:?}",
            shell.calls()
        );
        no_sqlite3(&shell);
    }

    // Tripwire: the copy has to take -wal and -shm when they are there. A
    // main-file-only cp is exactly the silent-prefix bug the row-count check
    // exists to catch, and catching it after the fact is not the same as
    // not doing it.
    #[test]
    fn the_copy_takes_wal_and_shm_sidecars_when_they_exist() {
        let shell = Fake::new(folding());
        let paths = seeded_paths(12, 12);
        let mut log = String::new();
        prove(&matching(), &shell, &paths, &mut log).expect("matching counts fold");
        assert!(log.contains("journal rows live=12 copy=12"), "equal counts are logged: {log}");

        let calls = shell.calls();
        for suffix in SIDECARS {
            let from = sidecar_path(&paths.store, suffix);
            assert!(
                calls.iter().any(|line| line.starts_with("cp ") && line.contains(&from.display().to_string())),
                "the {suffix} sidecar is copied: {calls:?}"
            );
        }
        no_sqlite3(&shell);
    }

    // Tripwire: a checkpointed store has no -wal/-shm. `cp` of a sidecar
    // that is not there is a failed Host invocation, which would refuse a
    // sound journal that happens to have flushed its WAL.
    #[test]
    fn a_checkpointed_store_is_copied_without_missing_sidecars() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("test -e") => Run::failed(""),
            _ => Run::ok(""),
        });
        let paths = seeded_paths(12, 12);
        let mut log = String::new();
        prove(&matching(), &shell, &paths, &mut log).expect("a checkpointed store folds");

        let calls = shell.calls();
        for suffix in SIDECARS {
            let from = sidecar_path(&paths.store, suffix);
            assert!(
                !calls.iter().any(|line| line.starts_with("cp ") && line.contains(&from.display().to_string())),
                "a missing {suffix} is not copied: {calls:?}"
            );
        }
        assert!(
            calls.iter().any(|line| line.starts_with("cp ") && line.contains(&paths.store.display().to_string())),
            "the main store file is still copied: {calls:?}"
        );
        no_sqlite3(&shell);
    }

    // Tripwire: a candidate whose wire cannot decode the journal must die at
    // the fold-test with the decode error in the refusal. Restarting first is
    // how #4942 bricked the live coordinator.
    #[test]
    fn a_replay_breaking_candidate_surfaces_the_decode_error() {
        let decode = "record 2 decision did not decode: aether wire: invalid bool/presence byte 11";
        let shell = Fake::new(move |line| match line {
            line if line.contains("--store-path") => Run::failed(decode),
            line if line.starts_with("test -e") => Run::ok(""),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal =
            prove(&matching(), &shell, &seeded_paths(12, 12), &mut log).expect_err("a reshape is refused").to_string();

        assert!(refusal.contains(decode), "the decode error is surfaced: {refusal}");
        assert!(refusal.contains("fold-test aborted"), "the refusal names the fold-test: {refusal}");
        no_sqlite3(&shell);
    }

    #[test]
    fn a_diverged_fold_names_both_states() {
        let mut folded = drained_view();
        folded.mainline = Digest::from_bytes([9; 32]);
        let views = Scripted::new(Ok(drained_view()), Ok(folded));
        let shell = Fake::new(folding());
        let mut log = String::new();

        let refusal =
            prove(&views, &shell, &seeded_paths(12, 12), &mut log).expect_err("a diverged fold is refused").to_string();

        assert!(refusal.contains("diverged"), "the refusal names the divergence: {refusal}");
        assert!(refusal.contains(&drained_view().mainline.to_string()), "live mainline is named: {refusal}");
        assert!(refusal.contains(&Digest::from_bytes([9; 32]).to_string()), "folded mainline is named: {refusal}");
        assert!(refusal.contains("blooms="), "both bloom counts are named: {refusal}");
    }

    // Tripwire: /view can bind before journal replay finishes. Treating that
    // first empty document as the fold result refuses a sound candidate with
    // a phantom divergence. The fold-test has to wait for the copy to match
    // live, which is how it knows replay actually completed.
    #[test]
    fn a_fold_that_catches_up_after_replay_is_not_a_divergence() {
        let mut empty = drained_view();
        empty.mainline = Digest::from_bytes([0; 32]);
        empty.blooms.clear();
        let views = Scripted::matching().first_folded(empty);
        let shell = Fake::new(folding());
        let mut paths = seeded_paths(12, 12);
        paths.fold_timeout_millis = 5_000;
        let mut log = String::new();

        prove(&views, &shell, &paths, &mut log).expect("a late-matching fold is the live journal");
        assert!(log.contains("matches live"), "the caught-up fold is what was checked: {log}");
    }

    // Tripwire: the fold-test has to boot the candidate against the copy, on
    // scratch ports, with lanes off. Launching against the live store, or
    // leaving lanes on, is how a fold-test writes through to the journal it
    // was supposed to leave untouched.
    #[test]
    fn the_fold_test_boots_the_copy_with_lanes_disabled() {
        let shell = Fake::new(folding());
        let paths = seeded_paths(12, 12);
        let mut log = String::new();
        prove(&matching(), &shell, &paths, &mut log).expect("matching counts fold");

        let launch =
            shell.calls().into_iter().find(|line| line.contains("--store-path")).expect("the candidate is launched");
        assert!(launch.contains("fold.db"), "the store is the copy: {launch}");
        assert!(
            !launch.contains(&paths.store.display().to_string()),
            "the live store is not the fold target: {launch}"
        );
        assert!(launch.contains("--github-local-lane-enabled=false"), "lanes are off: {launch}");
        assert!(launch.contains("--http-port 18910"), "scratch http port: {launch}");
        assert!(launch.contains("--rpc-port 18909"), "scratch rpc port: {launch}");
    }

    // Tripwire: a candidate that is serving while the live coordinator is
    // unreachable must time out, not spin. The previous match only terminated
    // when both /views were Ok or the candidate itself failed to serve, so
    // (folded Ok, live Err) looped past the deadline.
    #[test]
    fn a_live_unreachable_fold_times_out_rather_than_spinning() {
        let views = Scripted::new(Err("connection refused".to_owned()), Ok(drained_view()));
        let shell = Fake::new(folding());
        let mut log = String::new();

        let refusal = prove(&views, &shell, &seeded_paths(12, 12), &mut log)
            .expect_err("an unreachable live coordinator is a refusal")
            .to_string();

        assert!(refusal.contains("not yet"), "the stall is a timeout: {refusal}");
        assert!(refusal.contains("connection refused"), "the live error is surfaced: {refusal}");
    }
}
