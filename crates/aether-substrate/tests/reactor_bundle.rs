//! Generated reactor-bundle WASM: stream-bound Event/EventBatch, shared views,
//! guard decline, isolated clusters, and replacement reconstruction of
//! generated inline peers.
//!
//! Loads the compiled `aether_test_fixtures_bundle` wasm. A skip when the
//! artifact is absent is not proof of this wiring.

use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_bloomery_kinds::{Digest, Head, Program, Ref, Tree};
use aether_bloomery_reactor::{
    CLUSTER_NAMESPACE, ClusterConfig, ClusterStatus, ClusterStatusQuery, EvaluatedResult, Event, EventBatch,
    JournalEntry, PeerEvaluated, PreparedResult,
};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, MailboxId, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{CollectReactorOutputs, CollectReactorOutputsResult, REACTOR_FOLD_FAIL_KIND};

const SINK: &str = "test.bloomery.reactor.sink";
const STREAM_A: &str = "alpha";
const STREAM_B: &str = "beta";
/// Must match `SourcePublisher` / `SourceWitness` `NAMESPACE` in `reactor_cluster.rs`.
const SOURCE_PUBLISHER: &str = "test.bloomery.source.publisher";
const SOURCE_WITNESS: &str = "test.bloomery.source.witness";

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn journal_moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> JournalEntry {
    let event = Head::<K>::new(name).move_to(to);
    JournalEntry {
        seq,
        kind: aether_bloomery_kinds::HeadMoved::<K>::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: aether_bloomery_kinds::HeadMoved::<K>::encode_storage(&StorageData::from_value(event))
            .expect("storage encode"),
    }
}

fn fold_fail(seq: u64) -> JournalEntry {
    JournalEntry { seq, kind: REACTOR_FOLD_FAIL_KIND.to_owned(), cause: None, recorded_at_millis: 0, bytes: Vec::new() }
}

fn live_event<K: Kind + 'static>(stream: &str, seq: u64, name: &'static str, to: Ref<K>) -> Event {
    Event { stream: stream.to_owned(), entry: journal_moved(seq, name, to) }
}

fn load_export_result(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    name: &str,
    export: &str,
    config: Vec<u8>,
) -> LoadResult {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent { wasm, name: Some(name.to_owned()), config, export: Some(export.to_owned()) },
            ),
        )])
        .expect("load sequence")
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
}

fn load_export(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str, export: &str, config: Vec<u8>) -> String {
    match load_export_result(harness, wasm_path, name, export, config) {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
}

fn load_cluster(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str, sink: String) -> String {
    load_cluster_full(harness, wasm_path, name, sink).0
}

fn load_cluster_full(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    name: &str,
    sink: String,
) -> (String, MailboxId) {
    let config = ClusterConfig { output: sink.clone(), ack: sink };
    match load_export_result(harness, wasm_path, name, CLUSTER_NAMESPACE, config.encode_into_bytes()) {
        LoadResult::Ok { name, mailbox_id, .. } => (name, mailbox_id),
        LoadResult::Err { error } => panic!("load_component({name}): {error}"),
    }
}

fn replace_cluster(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    mailbox_id: MailboxId,
    sink: String,
) -> ReplaceResult {
    let wasm = fs::read(wasm_path).expect("re-read fixture wasm");
    let config = ClusterConfig { output: sink.clone(), ack: sink };
    harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &ReplaceComponent {
                    mailbox_id,
                    wasm,
                    drain_timeout_ms: None,
                    config: config.encode_into_bytes(),
                    export: Some(CLUSTER_NAMESPACE.to_owned()),
                },
            ),
        )])
        .expect("replace sequence")
        .reply::<ReplaceResult>("swap")
        .expect("decode ReplaceResult")
}

fn evaluated_ok_count(report: &CollectReactorOutputsResult, seq: u64) -> usize {
    report
        .evaluated
        .iter()
        .filter(|result| matches!(result, EvaluatedResult::Ok { seq: found, .. } if *found == seq))
        .count()
}

fn status(harness: &mut SubstrateHarness, address: &str) -> ClusterStatus {
    harness
        .execute(vec![("status", HarnessOp::send_and_await_reply(address, &ClusterStatusQuery))])
        .expect("status sequence")
        .reply::<ClusterStatus>("status")
        .expect("decode ClusterStatus")
}

fn collect(harness: &mut SubstrateHarness, sink: &str) -> CollectReactorOutputsResult {
    harness
        .execute(vec![("collect", HarnessOp::send_and_await_reply(sink, &CollectReactorOutputs))])
        .expect("collect sequence")
        .reply::<CollectReactorOutputsResult>("collect")
        .expect("decode CollectReactorOutputsResult")
}

fn settle_event(harness: &mut SubstrateHarness, address: &str, event: &Event) {
    harness.execute(vec![("event", HarnessOp::send_and_settle(address, event))]).expect("event settle");
}

fn settle_batch(harness: &mut SubstrateHarness, address: &str, batch: &EventBatch) {
    harness.execute(vec![("batch", HarnessOp::send_and_settle(address, batch))]).expect("batch settle");
}

fn prepared_reply(harness: &mut SubstrateHarness, address: &str, event: &Event) -> PreparedResult {
    harness
        .execute(vec![("prepared", HarnessOp::send_and_await_reply(address, event))])
        .expect("prepared reply")
        .reply::<PreparedResult>("prepared")
        .expect("decode PreparedResult")
}

fn prepared_ok_seqs(report: &CollectReactorOutputsResult) -> Vec<u64> {
    report
        .prepared
        .iter()
        .filter_map(|result| match result {
            PreparedResult::Ok { seq, .. } => Some(*seq),
            PreparedResult::Err { .. } => None,
        })
        .collect()
}

fn evaluated_ok_seqs(report: &CollectReactorOutputsResult) -> Vec<u64> {
    report
        .evaluated
        .iter()
        .filter_map(|result| match result {
            EvaluatedResult::Ok { seq, .. } => Some(*seq),
            EvaluatedResult::Err { .. } => None,
        })
        .collect()
}

fn boot() -> Option<(SubstrateHarness, PathBuf)> {
    let wasm_path = require_wasm("aether_test_fixtures_bundle")?;
    let harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    Some((harness, wasm_path))
}

/// Shared fold, successful guarded output, declined guard, and isolated clusters.
///
/// This fails if the two reactors construct separate `FoldTally` instances, if a
/// declined current-head guard still publishes or holds suspended work, if two
/// loaded clusters share views or output routes, or if mailing one cluster
/// advances the other.
#[test]
fn reactor_bundle_shared_fold_guard_and_isolated_clusters() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink_a = load_export(&mut harness, &wasm_path, "sink-a", SINK, Vec::new());
    let sink_b = load_export(&mut harness, &wasm_path, "sink-b", SINK, Vec::new());
    let cluster_a = load_cluster(&mut harness, &wasm_path, "reactor-a", sink_a.clone());
    let cluster_b = load_cluster(&mut harness, &wasm_path, "reactor-b", sink_b.clone());

    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    settle_event(&mut harness, &cluster_a, &live_event(STREAM_A, 1, "current", program));
    let after_current = collect(&mut harness, &sink_a);
    assert!(after_current.guarded.is_empty());
    assert!(after_current.open.is_empty());
    assert_eq!(
        evaluated_ok_seqs(&after_current),
        [1],
        "declined guards complete evaluation: {:?}",
        after_current.evaluated
    );
    assert_eq!(status(&mut harness, &cluster_a).cursor, 1);

    settle_event(&mut harness, &cluster_b, &live_event(STREAM_B, 1, "source", tree));
    let from_b = collect(&mut harness, &sink_b);
    assert!(
        from_b.guarded.is_empty(),
        "B must not see A's current head; a shared owner would resolve the guard: {:?}",
        from_b.guarded
    );
    assert_eq!(from_b.open.len(), 1, "{:?}", from_b.open);
    assert_eq!(from_b.open[0].digest, [2; 32]);
    assert_eq!(from_b.open[0].folds, 1);
    let fold_b = from_b.open[0].fold_id;

    settle_event(&mut harness, &cluster_a, &live_event(STREAM_A, 2, "source", tree));
    let from_a = collect(&mut harness, &sink_a);
    assert_eq!(from_a.guarded.len(), 1, "{:?}", from_a.guarded);
    assert_eq!(from_a.open.len(), 1, "{:?}", from_a.open);
    assert_eq!(from_a.guarded[0].digest, [2; 32]);
    assert_eq!(from_a.open[0].digest, [2; 32]);
    assert_eq!(from_a.guarded[0].folds, 2);
    assert_eq!(from_a.open[0].folds, 2);
    assert_eq!(
        from_a.guarded[0].fold_id, from_a.open[0].fold_id,
        "both reactors must consume the same FoldTally snapshot"
    );
    assert_eq!(evaluated_ok_seqs(&from_a), [1, 2]);

    let still_b = collect(&mut harness, &sink_b);
    assert_eq!(still_b.open.len(), 1);
    assert!(still_b.guarded.is_empty());
    assert_eq!(still_b.open[0].fold_id, fold_b);

    let b_status = status(&mut harness, &cluster_b);
    assert_eq!(b_status.cursor, 1, "mailing A must not advance B's views or routes");
    assert_eq!(b_status.stream, STREAM_B);
    assert!(!b_status.poisoned);

    settle_event(&mut harness, &cluster_b, &live_event(STREAM_B, 2, "current", program));
    let now_ready = collect(&mut harness, &sink_b);
    assert!(now_ready.guarded.is_empty(), "a declined event must not resume when its guard becomes ready");
    assert_eq!(now_ready.open.len(), 1);
    assert_eq!(evaluated_ok_seqs(&now_ready), [1, 2]);

    settle_event(&mut harness, &cluster_b, &live_event(STREAM_B, 3, "source", digest_ref::<Tree>(3)));
    let later = collect(&mut harness, &sink_b);
    assert_eq!(later.guarded.len(), 1, "only the new matching event may publish");
    assert_eq!(later.guarded[0].digest, [3; 32]);
    assert_eq!(later.guarded[0].folds, 3);
    assert_eq!(later.open.len(), 2);
    assert_eq!(evaluated_ok_seqs(&later), [1, 2, 3]);
}

/// Fold-only warmup consumes history without live reactions; the next live
/// event then evaluates against that prefix.
#[test]
fn reactor_bundle_warmup_then_live() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-warmup", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-warmup", sink.clone());
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let batch = EventBatch::from_journal(STREAM_A, vec![journal_moved(1, "current", program)]).expect("range");

    settle_batch(&mut harness, &cluster, &batch);
    let warmed = collect(&mut harness, &sink);
    assert!(warmed.guarded.is_empty(), "warmup must not evaluate arms: {:?}", warmed.guarded);
    assert!(warmed.open.is_empty(), "{:?}", warmed.open);
    assert_eq!(prepared_ok_seqs(&warmed), [1]);
    assert!(warmed.evaluated.is_empty(), "warmup must not emit evaluation: {:?}", warmed.evaluated);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 2, "source", tree));
    let live = collect(&mut harness, &sink);
    assert_eq!(live.guarded.len(), 1, "{:?}", live.guarded);
    assert_eq!(live.open.len(), 1, "{:?}", live.open);
    assert_eq!(live.guarded[0].folds, 2);
    assert_eq!(evaluated_ok_seqs(&live), [2]);
}

/// Warmup of a two-entry prefix as one batch matches two single-entry batches
/// once a later live event evaluates.
#[test]
fn reactor_bundle_batch_and_single_fold_equivalence() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink_batch = load_export(&mut harness, &wasm_path, "sink-batch", SINK, Vec::new());
    let sink_single = load_export(&mut harness, &wasm_path, "sink-single", SINK, Vec::new());
    let cluster_batch = load_cluster(&mut harness, &wasm_path, "reactor-batch", sink_batch.clone());
    let cluster_single = load_cluster(&mut harness, &wasm_path, "reactor-single", sink_single.clone());
    let current = digest_ref::<Program>(1);
    let extra = digest_ref::<Program>(4);
    let tree = digest_ref::<Tree>(2);
    let history = [journal_moved(1, "current", current), journal_moved(2, "other", extra)];

    settle_batch(
        &mut harness,
        &cluster_batch,
        &EventBatch::from_journal(STREAM_A, vec![history[0].clone(), history[1].clone()]).expect("batch range"),
    );
    settle_batch(
        &mut harness,
        &cluster_single,
        &EventBatch::from_journal(STREAM_B, vec![history[0].clone()]).expect("first"),
    );
    settle_batch(
        &mut harness,
        &cluster_single,
        &EventBatch::from_journal(STREAM_B, vec![history[1].clone()]).expect("second"),
    );

    settle_event(&mut harness, &cluster_batch, &live_event(STREAM_A, 3, "source", tree));
    settle_event(&mut harness, &cluster_single, &live_event(STREAM_B, 3, "source", tree));
    let from_batch = collect(&mut harness, &sink_batch);
    let from_single = collect(&mut harness, &sink_single);
    assert_eq!(from_batch.guarded.len(), 1);
    assert_eq!(from_single.guarded.len(), 1);
    assert_eq!(from_batch.guarded[0].digest, from_single.guarded[0].digest);
    assert_eq!(from_batch.open[0].digest, from_single.open[0].digest);
    assert_eq!(from_batch.guarded[0].folds, from_single.guarded[0].folds);
    assert_eq!(from_batch.guarded[0].folds, 3);
}

/// A wrong activation token is refused without advancing the bound cluster.
#[test]
fn reactor_bundle_stream_mismatch() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-stream", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-stream", sink.clone());
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 1, "current", program));
    let mismatch = prepared_reply(&mut harness, &cluster, &live_event(STREAM_B, 2, "source", tree));
    assert!(
        matches!(&mismatch, PreparedResult::Err { stream, seq: 1, message } if stream == STREAM_B && message.contains("stream mismatch")),
        "{mismatch:?}"
    );
    let report = collect(&mut harness, &sink);
    assert!(report.guarded.is_empty());
    assert!(report.open.is_empty());
    assert!(!evaluated_ok_seqs(&report).contains(&2));
    let cluster_status = status(&mut harness, &cluster);
    assert_eq!(cluster_status.cursor, 1);
    assert_eq!(cluster_status.stream, STREAM_A);
}

/// Gaps and duplicates are refused before fold and do not emit evaluation success.
#[test]
fn reactor_bundle_gaps_and_duplicates() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-gap", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-gap", sink.clone());
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 1, "current", program));
    let gap = prepared_reply(&mut harness, &cluster, &live_event(STREAM_A, 3, "source", tree));
    assert!(matches!(&gap, PreparedResult::Err { seq: 1, message, .. } if message.contains("gap")), "{gap:?}");
    let duplicate = prepared_reply(&mut harness, &cluster, &live_event(STREAM_A, 1, "current", program));
    assert!(
        matches!(&duplicate, PreparedResult::Err { seq: 1, message, .. } if message.contains("duplicate")),
        "{duplicate:?}"
    );
    let report = collect(&mut harness, &sink);
    assert!(report.guarded.is_empty());
    assert!(report.open.is_empty());
    assert_eq!(evaluated_ok_seqs(&report), [1]);
    assert_eq!(status(&mut harness, &cluster).cursor, 1);
}

/// A failed fold replies preparation error, never evaluation success, and poisons
/// the cluster against later live events.
#[test]
fn reactor_bundle_failed_fold_has_no_success_ack() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-fail", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-fail", sink.clone());
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 1, "current", program));
    let failed = prepared_reply(&mut harness, &cluster, &Event { stream: STREAM_A.to_owned(), entry: fold_fail(2) });
    assert!(matches!(&failed, PreparedResult::Err { seq: 2, message, .. } if message.contains("fold")), "{failed:?}");
    let after_fail = collect(&mut harness, &sink);
    assert!(!evaluated_ok_seqs(&after_fail).contains(&2), "{:?}", after_fail.evaluated);
    assert!(after_fail.guarded.is_empty());
    assert!(after_fail.open.is_empty());
    assert!(status(&mut harness, &cluster).poisoned);

    let later = prepared_reply(&mut harness, &cluster, &live_event(STREAM_A, 3, "source", tree));
    assert!(
        matches!(&later, PreparedResult::Err { message, .. } if message.contains("poisoned") && message.contains("seq 1")),
        "{later:?}"
    );
    assert_eq!(status(&mut harness, &cluster).cursor, 2, "prefix may have advanced past the last trusted seq");
    let after_later = collect(&mut harness, &sink);
    assert!(after_later.guarded.is_empty());
    assert!(after_later.open.is_empty());
    assert!(!evaluated_ok_seqs(&after_later).contains(&3));
}

/// Two later source moves keep the Heads prefix each event prepared.
#[test]
fn reactor_bundle_conflicting_live_events_retain_heads_prefixes() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-heads", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-heads", sink.clone());
    let program = digest_ref::<Program>(1);
    let first = digest_ref::<Tree>(2);
    let second = digest_ref::<Tree>(3);

    settle_batch(
        &mut harness,
        &cluster,
        &EventBatch::from_journal(STREAM_A, vec![journal_moved(1, "current", program)]).expect("warmup"),
    );
    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 2, "source", first));
    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 3, "source", second));
    let report = collect(&mut harness, &sink);
    assert_eq!(report.guarded.len(), 2, "{:?}", report.guarded);
    assert_eq!(report.open.len(), 2, "{:?}", report.open);
    assert_eq!(report.guarded[0].digest, [2; 32]);
    assert_eq!(report.open[0].digest, [2; 32]);
    assert_eq!(report.guarded[1].digest, [3; 32]);
    assert_eq!(report.open[1].digest, [3; 32]);
    assert_eq!(evaluated_ok_seqs(&report), [2, 3]);
}

/// A `PeerEvaluated` mail that is not from an expected cluster peer cannot create
/// or duplicate a successful evaluation.
#[test]
fn reactor_bundle_unknown_peer_outcome_is_not_success() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-forged", SINK, Vec::new());
    let cluster = load_cluster(&mut harness, &wasm_path, "reactor-forged", sink.clone());
    let program = digest_ref::<Program>(1);

    harness
        .execute(vec![("forged", HarnessOp::send_and_settle(&cluster, &PeerEvaluated::ok(STREAM_A, 1)))])
        .expect("forged peer settle");
    let before = collect(&mut harness, &sink);
    assert!(before.evaluated.is_empty(), "{:?}", before.evaluated);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 1, "current", program));
    let once = collect(&mut harness, &sink);
    assert_eq!(evaluated_ok_seqs(&once), [1]);

    harness
        .execute(vec![("again", HarnessOp::send_and_settle(&cluster, &PeerEvaluated::ok(STREAM_A, 1)))])
        .expect("duplicate forged peer settle");
    harness
        .execute(vec![("wrong-stream", HarnessOp::send_and_settle(&cluster, &PeerEvaluated::ok(STREAM_B, 1)))])
        .expect("wrong-stream peer settle");
    let still = collect(&mut harness, &sink);
    assert_eq!(evaluated_ok_seqs(&still), [1], "{:?}", still.evaluated);
}

/// Replacement reconstructs generated reactor peers at their original aliases.
/// Coordinator views rebuild (`init` without `wire`); fold-only warmup is
/// required before the next live event. A skip when the fixture wasm is
/// missing is not proof of this path.
#[test]
fn reactor_bundle_replace_restores_generated_peers() {
    let Some((mut harness, wasm_path)) = boot() else {
        return;
    };
    let sink = load_export(&mut harness, &wasm_path, "sink-replace", SINK, Vec::new());

    let publisher =
        load_export_result(&mut harness, &wasm_path, "must-not-spawn-publisher", SOURCE_PUBLISHER, Vec::new());
    assert!(
        matches!(publisher, LoadResult::Err { .. }),
        "generated publisher peers must not be independently spawnable: {publisher:?}"
    );
    let witness = load_export_result(&mut harness, &wasm_path, "must-not-spawn-witness", SOURCE_WITNESS, Vec::new());
    assert!(
        matches!(witness, LoadResult::Err { .. }),
        "generated witness peers must not be independently spawnable: {witness:?}"
    );

    let (cluster, mailbox_id) = load_cluster_full(&mut harness, &wasm_path, "reactor-replace", sink.clone());
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);

    for round in 1..=2 {
        match replace_cluster(&mut harness, &wasm_path, mailbox_id, sink.clone()) {
            ReplaceResult::Ok { .. } => {}
            ReplaceResult::Err { error } => panic!("replace_component round {round}: {error}"),
        }
    }
    let rebuilt = status(&mut harness, &cluster);
    assert_eq!(
        rebuilt.cursor, 0,
        "replacement rebuilds coordinator views; peer restore is not aggregation persistence"
    );
    assert!(!rebuilt.poisoned);
    assert_eq!(rebuilt.stream, "");

    settle_batch(
        &mut harness,
        &cluster,
        &EventBatch::from_journal(STREAM_A, vec![journal_moved(1, "current", program)]).expect("warmup"),
    );
    let warmed = collect(&mut harness, &sink);
    assert!(warmed.guarded.is_empty(), "warmup must not evaluate arms: {:?}", warmed.guarded);
    assert!(warmed.open.is_empty(), "{:?}", warmed.open);
    assert!(warmed.evaluated.is_empty(), "warmup must not emit evaluation: {:?}", warmed.evaluated);
    assert_eq!(prepared_ok_seqs(&warmed), [1]);

    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 2, "source", tree));
    let live = collect(&mut harness, &sink);
    assert_eq!(live.guarded.len(), 1, "restored publisher must evaluate once after replace: {:?}", live.guarded);
    assert_eq!(live.open.len(), 1, "restored witness must evaluate once after replace: {:?}", live.open);
    assert_eq!(live.guarded[0].digest, [2; 32]);
    assert_eq!(live.open[0].digest, [2; 32]);
    assert_eq!(live.guarded[0].fold_id, live.open[0].fold_id);
    assert_eq!(live.guarded[0].folds, 2);
    assert_eq!(evaluated_ok_count(&live, 2), 1, "exactly one evaluation ack after replace: {:?}", live.evaluated);
    assert_eq!(evaluated_ok_seqs(&live), [2]);

    match replace_cluster(&mut harness, &wasm_path, mailbox_id, sink.clone()) {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component after live: {error}"),
    }
    assert_eq!(status(&mut harness, &cluster).cursor, 0, "a later replace still rebuilds views");
    settle_batch(
        &mut harness,
        &cluster,
        &EventBatch::from_journal(STREAM_A, vec![journal_moved(1, "current", program)]).expect("second warmup"),
    );
    settle_event(&mut harness, &cluster, &live_event(STREAM_A, 2, "source", tree));
    let again = collect(&mut harness, &sink);
    assert_eq!(again.guarded.len(), 2, "second live event after repeated replace: {:?}", again.guarded);
    assert_eq!(again.open.len(), 2, "{:?}", again.open);
    assert_eq!(
        evaluated_ok_count(&again, 2),
        2,
        "one evaluation per live event, not a duplicate peer storm: {:?}",
        again.evaluated
    );
    assert_eq!(again.guarded[1].folds, 2, "successor views fold the warmup and live entry from empty state");
}
