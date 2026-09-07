//! Focused coverage for dispatch evidence reads: line-snapped paging, mtime
//! silence, swept-nonce honesty, and coordinator-log clamp/filter/page.

use std::fs;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::time::{Duration, SystemTime};

use aether_bloomery::{BloomId, Digest, Harness, MetricDispatch, ReasoningEffort, ResolvedModel, StageId};
use aether_data::wire::to_vec;

use super::header::{ASSISTANT_TEXT_CAP, read as read_header};
use super::list::assemble;
use super::logs::{COORDINATOR_LOG_MAX, LogError, LogQuery, read_with};
use super::ranged::{FileQuery, TRANSCRIPT_DEFAULT_LIMIT, TRANSCRIPT_LINE_CAP, TRANSCRIPT_MAX_LIMIT, read_ranged};
use super::{SWEPT_NOTICE, evidence_dir};
use crate::api::dto::CoordinatorLogsView;
use crate::store::{BloomDispatchLive, BloomDispatchRollup};

#[test]
fn ranged_reads_snap_both_ends_to_line_boundaries() {
    // A mid-line cursor must not return a partial first or last line. Paging
    // then walks purely by the returned cursors.
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let path = dir.path().join("transcript.jsonl");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("the fixture writes");

    let first = read_ranged(&path, Some(1), 8).expect("an in-range read succeeds");
    assert_eq!(first.lines, vec!["two".to_owned()], "limit 8 from mid-'one' yields the next complete line only");
    assert_eq!(first.cursor, 4, "offset 1 sits inside 'one\\n'; start snaps to 'two'");
    assert_eq!(first.length, 24);

    let second = read_ranged(&path, first.next_cursor, 8).expect("the next cursor pages");
    assert_eq!(second.lines, vec!["three".to_owned()]);

    let mut seen = first.lines;
    seen.extend(second.lines);
    let mut cursor = second.next_cursor;
    while let Some(from) = cursor {
        let page = read_ranged(&path, Some(from), 8).expect("later pages succeed");
        seen.extend(page.lines);
        cursor = page.next_cursor;
    }
    assert_eq!(seen, vec!["two", "three", "four", "five"], "cursors walk every complete line after the snapped start");
}

#[test]
fn a_transcript_read_leaves_mtime_untouched() {
    // ADR-0195 §8: mtime is the executor's live-progress signal. A header or
    // transcript read that wrote metadata would look like the lane is still
    // making progress.
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let path = dir.path().join("transcript.jsonl");
    fs::write(&path, "alpha\nbeta\n").expect("the fixture writes");
    let pinned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = fs::File::options().write(true).open(&path).expect("the fixture opens for mtime pin");
    file.set_modified(pinned).expect("mtime pins");
    drop(file);

    let _page = read_ranged(&path, None, 64).expect("the tail read succeeds");
    let after = fs::metadata(&path).expect("metadata is readable").modified().expect("mtime is readable");
    assert_eq!(after, pinned, "a ranged read must not write the file's mtime");
}

#[test]
fn a_swept_nonce_reports_retained_false_with_a_notice() {
    // The journal named this nonce; the directory is gone from both the
    // working root and the archive tier. A disk client would 404 and cannot
    // tell absence from "never existed".
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let view = read_header(dir.path(), dir.path(), "dispatch-9");
    assert!(!view.retained);
    assert_eq!(view.notice.as_deref(), Some(SWEPT_NOTICE));
    assert!(view.files.is_empty());
    assert_eq!(view.archived, None);
}

#[test]
fn an_archived_nonce_reads_as_retained_with_its_tier_path() {
    // ADR-0211: a record that moved to the tier must not read as swept. The
    // header reports retained, names the tier path, and still lists files.
    let work = tempfile::tempdir().expect("a working root is available");
    let archive = tempfile::tempdir().expect("an archive root is available");
    let evidence = archive.path().join("evidence").join("dispatch-9-evidence");
    fs::create_dir_all(&evidence).expect("the archived evidence directory is created");
    fs::write(evidence.join("transcript.jsonl"), "hello\n").expect("the transcript writes");

    let view = read_header(work.path(), archive.path(), "dispatch-9");
    assert!(view.retained, "an archived record is still here");
    assert_eq!(view.notice, None, "archived is not swept");
    assert_eq!(view.archived.as_deref(), Some(evidence.to_str().expect("the archive path is UTF-8")));
    assert!(view.files.iter().any(|name| name == "transcript.jsonl"));
}

#[test]
fn assistant_text_and_commit_message_cap_independently() {
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let evidence = evidence_dir(dir.path(), "dispatch-1");
    fs::create_dir_all(&evidence).expect("the evidence directory is created");
    let assistant = "a".repeat(ASSISTANT_TEXT_CAP + 32);
    fs::write(
        evidence.join("evidence.json"),
        format!(r#"{{"assistant_text":"{assistant}","commit_message":"fix(store): join"}}"#),
    )
    .expect("the evidence header writes");

    let view = read_header(dir.path(), dir.path(), "dispatch-1");
    assert!(view.retained);
    assert!(view.assistant_text_truncated);
    assert_eq!(view.assistant_text.as_ref().map(String::len), Some(ASSISTANT_TEXT_CAP));
    assert_eq!(view.commit_message.as_deref(), Some("fix(store): join"));
    assert!(!view.commit_message_truncated);
}

#[test]
fn cost_is_null_without_a_study_record_and_never_a_synthesized_zero() {
    // The plausible bug: treating a missing study as cost 0, which is how an
    // unpriced attempt is spelled and would hide the gap.
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let payload = to_vec(&MetricDispatch {
        id: "fold:x".to_owned(),
        bloom: BloomId(Digest::from_bytes([1; 32])),
        workpiece: "issue-1".to_owned(),
        stage: StageId::Construct,
        displayed: Digest::from_bytes([2; 32]),
        sequence: 3,
        recorded_unix_millis: None,
        reconstructed: true,
        agent: ResolvedModel { harness: Harness::Claude, model: "x".to_owned(), effort: ReasoningEffort::Low },
        study: None,
    })
    .expect("a metric row encodes");
    let view = assemble(
        dir.path(),
        dir.path(),
        None,
        &[BloomDispatchRollup { nonce: "dispatch-3".to_owned(), sequence: 3, payload }],
        &[],
    );
    assert_eq!(view.dispatches.len(), 1);
    assert_eq!(view.dispatches[0].cost, None, "no study record must not become 0");
    assert_eq!(view.dispatches[0].attempt, 1);
}

#[test]
fn live_outstanding_joins_the_rollup_and_keeps_its_nonce() {
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let payload = to_vec(&MetricDispatch {
        id: "fold:x".to_owned(),
        bloom: BloomId(Digest::from_bytes([1; 32])),
        workpiece: "issue-1".to_owned(),
        stage: StageId::Construct,
        displayed: Digest::from_bytes([2; 32]),
        sequence: 3,
        recorded_unix_millis: None,
        reconstructed: true,
        agent: ResolvedModel { harness: Harness::Claude, model: "x".to_owned(), effort: ReasoningEffort::Low },
        study: None,
    })
    .expect("a metric row encodes");
    let view = assemble(
        dir.path(),
        dir.path(),
        None,
        &[BloomDispatchRollup { nonce: "fold:x".to_owned(), sequence: 3, payload }],
        &[BloomDispatchLive {
            nonce: "dispatch-3".to_owned(),
            workpiece: "issue-1".to_owned(),
            stage: to_vec(&StageId::Construct).expect("stage encodes"),
            displayed: Digest::from_bytes([2; 32]).as_bytes().to_vec(),
        }],
    );
    assert_eq!(view.dispatches.len(), 1, "the live order overlays the fold-id row, it does not duplicate it");
    assert_eq!(view.dispatches[0].nonce, "dispatch-3");
}

#[test]
fn percent_decode_is_utf8_over_the_whole_byte_string() {
    // The plausible bug: decoding each `%HH` as a char (Latin-1) turns
    // `%C3%A9` (UTF-8 é) into U+00C3 U+00A9.
    let query = LogQuery::parse("contains=%C3%A9").expect("a valid escape parses");
    assert_eq!(query.contains.as_deref(), Some("é"));
}

fn coordinator_jsonl() -> &'static str {
    concat!(
        r#"{"PRIORITY":"6","MESSAGE":"keep-one","__CURSOR":"c1","__REALTIME_TIMESTAMP":"1"}"#,
        "\n",
        "not-json\n",
        r#"{"PRIORITY":"7","MESSAGE":"keep-debug","__CURSOR":"c-debug","__REALTIME_TIMESTAMP":"9"}"#,
        "\n",
        r#"{"PRIORITY":"3","MESSAGE":"keep-err","__CURSOR":"c2","__REALTIME_TIMESTAMP":"2"}"#,
        "\n",
        r#"{"MESSAGE":"keep-missing-cursor"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"drop-me","__CURSOR":"c3","__REALTIME_TIMESTAMP":"3"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"keep-two","__CURSOR":"c4","__REALTIME_TIMESTAMP":"4"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"keep-three","__CURSOR":"c5","__REALTIME_TIMESTAMP":"5"}"#,
        "\n",
    )
}

#[cfg(unix)]
fn exit_status(code: u8) -> ExitStatus {
    ExitStatus::from_raw(i32::from(code) << 8)
}

#[cfg(windows)]
fn exit_status(code: u8) -> ExitStatus {
    ExitStatus::from_raw(u32::from(code))
}

fn journalctl_output(code: u8, stdout: &str, stderr: &str) -> Output {
    // Built here so the suite never spawns journalctl. Unix wait status stores
    // the exit code in the high byte; Windows uses the code directly.
    Output { status: exit_status(code), stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec() }
}

fn logs_ok(query: &str, stdout: &str) -> CoordinatorLogsView {
    read_with(query, || true, |_| Ok(journalctl_output(0, stdout, "")))
        .expect("a present host with a successful runner answers")
}

#[test]
fn coordinator_logs_clamp_filter_and_page() {
    // Filter then page on the production path: debug rows drop at info, contains
    // drops non-matches, malformed JSONL is skipped, and the cursor names the
    // last kept row rather than a raw journalctl line.
    let jsonl = coordinator_jsonl();
    let mut argv = None;
    let paged = read_with(
        "limit=2&contains=keep&level=info",
        || true,
        |got| {
            argv = Some(got.to_vec());
            Ok(journalctl_output(0, jsonl, ""))
        },
    )
    .expect("a present host answers");
    let argv = argv.expect("a successful host gate invokes the runner");
    assert!(
        argv.windows(2).any(|pair| pair[0] == "-n" && pair[1] == COORDINATOR_LOG_MAX.to_string()),
        "journalctl fetches the ceiling; the query limit pages after filter: {argv:?}"
    );
    assert!(argv.iter().any(|flag| flag == "--output=json"), "entries are parsed as journalctl JSONL: {argv:?}");
    assert_eq!(paged.entries.len(), 2);
    assert_eq!(paged.entries[0].message, "keep-one");
    assert_eq!(paged.entries[1].message, "keep-err");
    assert!(paged.truncated);
    assert_eq!(paged.next_cursor.as_deref(), Some("c2"));
    assert!(paged.entries.iter().all(|entry| entry.message.contains("keep")));
    assert!(!paged.entries.iter().any(|entry| entry.message == "keep-debug"));
    assert!(!paged.entries.iter().any(|entry| entry.message == "keep-missing-cursor"));
    assert!(paged.notice.is_none());

    let debug = logs_ok("contains=keep&level=debug", jsonl);
    assert!(debug.entries.iter().any(|entry| entry.message == "keep-debug"));
    assert!(!debug.truncated);
    assert_eq!(debug.next_cursor, None);

    let clamped = logs_ok(&format!("limit={}&contains=keep&level=info", COORDINATOR_LOG_MAX + 50), jsonl);
    assert_eq!(
        clamped.entries.iter().map(|entry| entry.message.as_str()).collect::<Vec<_>>(),
        ["keep-one", "keep-err", "keep-two", "keep-three"]
    );
    assert!(!clamped.truncated);
    assert_eq!(clamped.next_cursor, None);
    assert!(clamped.notice.as_deref().is_some_and(|notice| notice.contains("clamped")));
}

#[test]
fn coordinator_logs_refuse_a_bad_query_before_the_host_gate() {
    // An unknown level is a 400. Probing systemd or invoking journalctl would
    // turn a bad query into a host-dependent 501/500.
    let error = read_with(
        "level=fatal",
        || panic!("host gate must not run on a bad query"),
        |_| panic!("runner must not run on a bad query"),
    )
    .expect_err("an unknown level is a query refusal");
    match error {
        LogError::BadQuery(message) => assert!(message.contains("fatal"), "{message}"),
        other => panic!("expected BadQuery, got {other:?}"),
    }
}

#[test]
fn coordinator_logs_are_unavailable_without_systemd() {
    // Without journald the route fails closed rather than spawning journalctl.
    let error = read_with("", || false, |_| panic!("runner must not run when systemd is absent"))
        .expect_err("a host without systemd cannot answer");
    match error {
        LogError::Unavailable { reason } => {
            assert!(reason.contains("systemd"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn coordinator_logs_surface_a_failed_journalctl_status() {
    // A non-zero journalctl is an IO error; stderr must reach the caller.
    let error = read_with("", || true, |_| Ok(journalctl_output(1, "", "unit not found")))
        .expect_err("a non-zero journalctl is an IO error");
    match error {
        LogError::Io(message) => assert!(message.contains("unit not found"), "{message}"),
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn a_transcript_limit_above_the_clamp_is_applied_and_named() {
    let query = FileQuery::parse(&format!("limit={}", TRANSCRIPT_MAX_LIMIT + 1)).expect("numeric limit parses");
    assert_eq!(query.limit, TRANSCRIPT_MAX_LIMIT);
    assert!(query.notice.as_deref().is_some_and(|notice| notice.contains("clamped")));
    assert_eq!(FileQuery::parse("").expect("empty query is valid").limit, TRANSCRIPT_DEFAULT_LIMIT);
}

#[test]
fn a_per_line_cap_truncates_the_rendered_line_and_still_advances() {
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let path = dir.path().join("transcript.jsonl");
    let huge = "x".repeat(TRANSCRIPT_LINE_CAP + 40);
    fs::write(&path, format!("{huge}\nnext\n")).expect("the fixture writes");
    let page = read_ranged(&path, Some(0), TRANSCRIPT_MAX_LIMIT).expect("the over-long line is readable");
    assert_eq!(page.lines[0].len(), TRANSCRIPT_LINE_CAP);
    assert_eq!(page.lines[1], "next");
    assert_eq!(page.next_cursor, None);
}

#[test]
fn a_line_longer_than_the_page_budget_still_advances() {
    // The LINE_CAP test uses a page much larger than its line, so it never
    // hits the stall: limit+1 bytes with no newline consumed 0 and returned
    // the input cursor forever. The following line must remain reachable.
    let dir = tempfile::tempdir().expect("a scratch directory is available");
    let path = dir.path().join("transcript.jsonl");
    let limit = 32_u64;
    let huge = "x".repeat(128);
    fs::write(&path, format!("{huge}\nnext\n")).expect("the fixture writes");

    let first = read_ranged(&path, Some(0), limit).expect("the over-budget line is readable");
    assert_eq!(first.cursor, 0);
    assert_eq!(first.lines.len(), 1, "the oversized line still renders a capped prefix");
    assert!(!first.lines[0].is_empty());
    assert!(first.lines[0].chars().all(|ch| ch == 'x'));
    assert!(first.lines[0].len() < huge.len(), "the prefix must not be the whole over-budget line");
    let next = first.next_cursor.expect("the page must name a following cursor");
    assert!(next > first.cursor, "next_cursor must strictly advance");

    let mut seen = first.lines;
    let mut cursor = Some(next);
    let mut steps = 0_u32;
    while let Some(from) = cursor {
        steps += 1;
        assert!(steps < 8, "pagination must not stall on a line larger than the page");
        let page = read_ranged(&path, Some(from), limit).expect("later pages succeed");
        assert_eq!(page.cursor, from, "next_cursor must land on a line start so a follow-up page is not snapped away");
        assert_ne!(page.next_cursor, Some(from), "next_cursor must not repeat the input");
        seen.extend(page.lines);
        cursor = page.next_cursor;
    }
    assert!(seen.iter().any(|line| line == "next"), "later complete lines must remain reachable");
}
