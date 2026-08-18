//! Focused coverage for dispatch evidence reads: line-snapped paging, mtime
//! silence, swept-nonce honesty, and coordinator-log clamp/filter/page.

use std::fs;
use std::time::{Duration, SystemTime};

use aether_bloomery::{BloomId, Digest, Harness, MetricDispatch, ReasoningEffort, ResolvedModel, StageId};
use aether_data::wire::to_vec;

use super::header::{ASSISTANT_TEXT_CAP, read as read_header};
use super::list::assemble;
use super::logs::{COORDINATOR_LOG_MAX, LogQuery, page_entries};
use super::ranged::{FileQuery, TRANSCRIPT_DEFAULT_LIMIT, TRANSCRIPT_LINE_CAP, TRANSCRIPT_MAX_LIMIT, read_ranged};
use super::{SWEPT_NOTICE, evidence_dir};
use crate::store::{BloomDispatchLive, BloomDispatchRollup};

#[test]
fn ranged_reads_snap_both_ends_to_line_boundaries() {
    // A mid-line cursor must not return a partial first or last line. Paging
    // then walks purely by the returned cursors.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    fs::write(&path, "alpha\nbeta\n").unwrap();
    let pinned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(pinned).unwrap();
    drop(file);

    let _page = read_ranged(&path, None, 64).expect("the tail read succeeds");
    let after = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(after, pinned, "a ranged read must not write the file's mtime");
}

#[test]
fn a_swept_nonce_reports_retained_false_with_a_notice() {
    // The journal named this nonce; the janitor deleted the directory. A disk
    // client would 404 and cannot tell absence from "never existed".
    let dir = tempfile::tempdir().unwrap();
    let view = read_header(dir.path(), "dispatch-9");
    assert!(!view.retained);
    assert_eq!(view.notice.as_deref(), Some(SWEPT_NOTICE));
    assert!(view.files.is_empty());
}

#[test]
fn assistant_text_and_commit_message_cap_independently() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = evidence_dir(dir.path(), "dispatch-1");
    fs::create_dir_all(&evidence).unwrap();
    let assistant = "a".repeat(ASSISTANT_TEXT_CAP + 32);
    fs::write(
        evidence.join("evidence.json"),
        format!(r#"{{"assistant_text":"{assistant}","commit_message":"fix(store): join"}}"#),
    )
    .unwrap();

    let view = read_header(dir.path(), "dispatch-1");
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
fn coordinator_logs_clamp_filter_and_page() {
    let query = LogQuery::parse(&format!("limit={}&contains=keep&level=info", COORDINATOR_LOG_MAX + 50))
        .expect("a numeric over-cap limit parses");
    assert_eq!(query.limit, COORDINATOR_LOG_MAX);
    assert!(query.notice.as_deref().is_some_and(|notice| notice.contains("clamped")));

    let jsonl = concat!(
        r#"{"PRIORITY":"6","MESSAGE":"keep-one","__CURSOR":"c1","__REALTIME_TIMESTAMP":"1"}"#,
        "\n",
        r#"{"PRIORITY":"3","MESSAGE":"keep-err","__CURSOR":"c2","__REALTIME_TIMESTAMP":"2"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"drop-me","__CURSOR":"c3","__REALTIME_TIMESTAMP":"3"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"keep-two","__CURSOR":"c4","__REALTIME_TIMESTAMP":"4"}"#,
        "\n",
        r#"{"PRIORITY":"6","MESSAGE":"keep-three","__CURSOR":"c5","__REALTIME_TIMESTAMP":"5"}"#,
        "\n",
    );
    let paged = page_entries(&LogQuery { limit: 2, ..query }, jsonl);
    assert_eq!(paged.entries.len(), 2);
    assert_eq!(paged.entries[0].message, "keep-one");
    assert_eq!(paged.entries[1].message, "keep-err");
    assert!(paged.truncated);
    assert_eq!(paged.next_cursor.as_deref(), Some("c2"));
    assert!(paged.entries.iter().all(|entry| entry.message.contains("keep")));
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let huge = "x".repeat(TRANSCRIPT_LINE_CAP + 40);
    fs::write(&path, format!("{huge}\nnext\n")).unwrap();
    let page = read_ranged(&path, Some(0), TRANSCRIPT_MAX_LIMIT).expect("the over-long line is readable");
    assert_eq!(page.lines[0].len(), TRANSCRIPT_LINE_CAP);
    assert_eq!(page.lines[1], "next");
    assert_eq!(page.next_cursor, None);
}
