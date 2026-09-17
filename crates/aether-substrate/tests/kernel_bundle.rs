//! Actual generated kernel bundle: warmup, live intents, and peer restoration.
//!
//! CI builds `aether_bloomery_kernel` for WASM before this test. A missing
//! artifact is a test failure when `AETHER_REQUIRE_RUNTIME=1`.

// The native actor handler ABI owns decoded mail values.
#![allow(clippy::needless_pass_by_value)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_actor::Addressable;
use aether_bloomery_kernel::KernelReconcileIntent;
use aether_bloomery_kinds::{Digest, Head, HeadMoved, KERNEL_HEAD, OpaqueBytes, REACTORS_HEAD, ReactorSet, Ref, Tree};
use aether_bloomery_reactor::{
    CLUSTER_NAMESPACE, ClusterConfig, EvaluatedResult, Event, EventBatch, JournalEntry, PreparedResult,
};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, MailboxId, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};

const STREAM: &str = "kernel-test";

#[derive(Default)]
struct Observed {
    intents: Vec<u64>,
    prepared: Vec<PreparedResult>,
    evaluated: Vec<EvaluatedResult>,
}

struct KernelSink(Arc<Mutex<Observed>>);

#[aether_actor::actor(singleton, root)]
impl NativeActor for KernelSink {
    type Config = ();
    type Params = Arc<Mutex<Observed>>;
    const NAMESPACE: &'static str = "test.bloomery.kernel_sink";

    fn init((): (), observed: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self(observed))
    }

    #[aether_actor::handler::single]
    fn on_intent(&self, _ctx: &mut NativeCtx<'_>, intent: KernelReconcileIntent) {
        self.0.lock().expect("sink lock").intents.push(intent.event_seq);
    }

    #[aether_actor::handler::single]
    fn on_prepared(&self, _ctx: &mut NativeCtx<'_>, result: PreparedResult) {
        self.0.lock().expect("sink lock").prepared.push(result);
    }

    #[aether_actor::handler::single]
    fn on_evaluated(&self, _ctx: &mut NativeCtx<'_>, result: EvaluatedResult) {
        self.0.lock().expect("sink lock").evaluated.push(result);
    }
}

fn reference<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn moved<K: Kind + 'static>(seq: u64, head: Head<K>, to: Ref<K>) -> JournalEntry {
    JournalEntry {
        seq,
        kind: HeadMoved::<K>::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<K>::encode_storage(&StorageData::from_value(head.move_to(to))).expect("storage encode"),
    }
}

fn boot() -> Option<(SubstrateHarness, PathBuf, Arc<Mutex<Observed>>)> {
    let wasm_path = require_wasm("aether_bloomery_kernel")?;
    let observed = Arc::new(Mutex::new(Observed::default()));
    let harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_actor::<KernelSink>(Arc::clone(&observed))
        .with_component_host()
        .build()
        .expect("boot");
    Some((harness, wasm_path, observed))
}

fn load_cluster(harness: &mut SubstrateHarness, wasm_path: &Path, name: &str) -> (String, MailboxId) {
    let wasm = fs::read(wasm_path).expect("read kernel wasm");
    let sink = KernelSink::NAMESPACE.to_owned();
    let config = ClusterConfig { output: sink.clone(), ack: sink };
    let result = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some(CLUSTER_NAMESPACE.to_owned()),
                },
            ),
        )])
        .expect("load sequence")
        .reply::<LoadResult>("load")
        .expect("decode load result");
    match result {
        LoadResult::Ok { name, mailbox_id, .. } => (name, mailbox_id),
        LoadResult::Err { error } => panic!("load kernel: {error}"),
    }
}

fn replace_cluster(harness: &mut SubstrateHarness, wasm_path: &Path, mailbox_id: MailboxId) {
    let wasm = fs::read(wasm_path).expect("read replacement kernel wasm");
    let sink = KernelSink::NAMESPACE.to_owned();
    let config = ClusterConfig { output: sink.clone(), ack: sink };
    let result = harness
        .execute(vec![(
            "replace",
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
        .reply::<ReplaceResult>("replace")
        .expect("decode replace result");
    assert!(matches!(&result, ReplaceResult::Ok { .. }), "{result:?}");
}

fn settle_event(harness: &mut SubstrateHarness, address: &str, entry: JournalEntry) {
    harness
        .execute(vec![("event", HarnessOp::send_and_settle(address, &Event { stream: STREAM.to_owned(), entry }))])
        .expect("live event settles");
}

fn settle_batch(harness: &mut SubstrateHarness, address: &str, entries: Vec<JournalEntry>) {
    let batch = EventBatch::from_journal(STREAM, entries).expect("dense warmup");
    harness.execute(vec![("batch", HarnessOp::send_and_settle(address, &batch))]).expect("warmup settles");
}

fn evaluated_sequences(observed: &Observed) -> Vec<u64> {
    observed
        .evaluated
        .iter()
        .map(|result| match result {
            EvaluatedResult::Ok { stream, seq } if stream == STREAM => *seq,
            other => panic!("unexpected evaluation result: {other:?}"),
        })
        .collect()
}

#[test]
fn kernel_warmup_is_silent_and_live_moves_emit_one_correlated_intent() {
    let Some((mut harness, wasm_path, observed)) = boot() else {
        return;
    };
    let (cluster, _) = load_cluster(&mut harness, &wasm_path, "kernel-live");

    settle_batch(
        &mut harness,
        &cluster,
        vec![moved(1, REACTORS_HEAD, reference(1)), moved(2, KERNEL_HEAD, reference(2))],
    );
    {
        let captured = observed.lock().expect("sink lock");
        assert!(captured.intents.is_empty(), "historical moves must not emit intents");
        assert!(captured.evaluated.is_empty(), "warmup does not evaluate peers");
        assert!(matches!(captured.prepared.as_slice(), [PreparedResult::Ok { stream, seq: 2 }] if stream == STREAM));
    }

    settle_event(&mut harness, &cluster, moved(3, REACTORS_HEAD, reference(3)));
    settle_event(&mut harness, &cluster, moved(4, KERNEL_HEAD, reference(4)));
    settle_event(&mut harness, &cluster, moved(5, Head::<Tree>::new("source"), reference(5)));

    let captured = observed.lock().expect("sink lock");
    assert_eq!(captured.intents, [3, 4]);
    assert_eq!(evaluated_sequences(&captured), [3, 4, 5]);
}

#[test]
fn kernel_replacement_restores_generated_peers_and_warmup_is_silent() {
    let Some((mut harness, wasm_path, observed)) = boot() else {
        return;
    };
    let (cluster, mailbox_id) = load_cluster(&mut harness, &wasm_path, "kernel-replace");
    let history = vec![moved(1, REACTORS_HEAD, reference(1)), moved(2, KERNEL_HEAD, reference(2))];
    settle_batch(&mut harness, &cluster, history.clone());
    let first_live = moved(3, Head::<OpaqueBytes>::new("worker"), reference(3));
    settle_event(&mut harness, &cluster, first_live.clone());
    assert_eq!(observed.lock().expect("sink lock").intents, [3]);

    replace_cluster(&mut harness, &wasm_path, mailbox_id);
    settle_batch(&mut harness, &cluster, [history, vec![first_live]].concat());
    assert_eq!(observed.lock().expect("sink lock").intents, [3], "warmup after replacement must not replay output");

    settle_event(&mut harness, &cluster, moved(4, KERNEL_HEAD, reference(4)));
    let captured = observed.lock().expect("sink lock");
    assert_eq!(captured.intents, [3, 4], "restored peer must emit once");
    assert_eq!(evaluated_sequences(&captured), [3, 4]);
}
