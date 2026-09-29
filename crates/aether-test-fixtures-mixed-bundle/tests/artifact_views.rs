//! Exercise resolver reply authentication and cancellation through real WASM.

use std::error::Error;
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};

use aether_actor::{ErasedActorRef, OutboundReply, actor};
use aether_bloomery_kinds::{
    BUNDLE_NAMESPACE, ClosureArtifact, EncodedArtifact, Evaluated, Event, HeadMoved, Invoke, Invoked, JournalEntry,
    ProgramName, ReadArtifact, ReadArtifactResult, Ref, Status, StatusQuery, Utf8Text, Warm, WarmEntries, Warmed,
};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, Source, Storage, StorageData};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};
use aether_test_fixtures_kinds::{
    ASYNC_SUMMARIZE_PROGRAM, RESOLVER_INPUT, ResolverReceipt, ResolverValue, SummarizeInput,
};

#[aether_data::kind(name = "test.resolver.start", no_serde)]
struct Start;

#[aether_data::kind(name = "test.resolver.start_invoke", no_serde)]
struct StartInvoke;

#[aether_data::kind(name = "test.resolver.start_warm", copy, no_serde)]
struct StartWarm {
    batch: u32,
}

#[aether_data::kind(name = "test.resolver.respond", copy, no_serde)]
struct Respond {
    request: u32,
    correlation_offset: u64,
}

#[aether_data::kind(name = "test.resolver.ack", no_serde)]
struct Ack;

#[aether_data::kind(name = "test.resolver.probe", no_serde)]
struct Probe;

#[aether_data::kind(name = "test.resolver.counts", no_serde)]
struct Counts {
    reads: u32,
    outcomes: u32,
    invocations: u32,
    warmed: u32,
}

#[derive(Default)]
struct Observed {
    reads: Vec<(Source, ReadArtifact)>,
    outcomes: Vec<Evaluated>,
    invocations: Vec<Invoked>,
    warmed: Vec<Warmed>,
}

#[derive(Clone)]
struct Shared {
    root: Arc<OnceLock<ErasedActorRef>>,
    observed: Arc<Mutex<Observed>>,
    artifacts: Arc<Vec<ClosureArtifact>>,
    event: Event,
    invoke: Option<Arc<Invoke>>,
    warms: Arc<Vec<Warm>>,
}

fn respond<A>(shared: &Shared, ctx: &mut NativeCtx<'_, A, aether_actor::Manual>, command: Respond) {
    let (source, request) = shared.observed.lock().expect("observed").reads[command.request as usize];
    let artifact = shared
        .artifacts
        .iter()
        .find(|artifact| artifact.claimed().unverified() == request.digest)
        .expect("requested artifact")
        .clone();
    ctx.reply_to(
        Source::with_correlation(source.addr, source.correlation_id + command.correlation_offset),
        &ReadArtifactResult::Found { artifact },
    );
    ctx.reply(&Ack);
}

struct DriverPeer(Shared);

#[actor(root)]
impl NativeActor for DriverPeer {
    type Config = ();
    type Params = Shared;
    const NAMESPACE: &'static str = "test.resolver.driver";

    fn init((): (), shared: Shared, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self(shared))
    }

    #[handler::single]
    fn on_start(&mut self, ctx: &mut NativeCtx<'_>, _start: Start) -> Ack {
        ctx.send_to(*self.0.root.get().expect("loaded root"), &self.0.event);
        Ack
    }

    #[handler::single]
    fn on_start_invoke(&mut self, ctx: &mut NativeCtx<'_>, _start: StartInvoke) -> Ack {
        ctx.send_to(*self.0.root.get().expect("loaded root"), self.0.invoke.as_deref().expect("configured invocation"));
        Ack
    }

    #[handler::single]
    fn on_start_warm(&mut self, ctx: &mut NativeCtx<'_>, start: StartWarm) -> Ack {
        ctx.send_to(*self.0.root.get().expect("loaded root"), &self.0.warms[start.batch as usize]);
        Ack
    }

    #[handler::manual]
    fn on_read(&mut self, ctx: &mut NativeCtx<'_, aether_actor::Erased, aether_actor::Manual>, read: ReadArtifact) {
        self.0.observed.lock().expect("observed").reads.push((ctx.reply_target(), read));
    }

    #[handler::manual]
    fn on_respond(&mut self, ctx: &mut NativeCtx<'_, aether_actor::Erased, aether_actor::Manual>, command: Respond) {
        respond(&self.0, ctx, command);
    }

    #[handler::single]
    fn on_evaluated(&mut self, _ctx: &mut NativeCtx<'_>, outcome: Evaluated) {
        self.0.observed.lock().expect("observed").outcomes.push(outcome);
    }

    #[handler::single]
    fn on_invoked(&mut self, _ctx: &mut NativeCtx<'_>, outcome: Invoked) {
        self.0.observed.lock().expect("observed").invocations.push(outcome);
    }

    #[handler::single]
    fn on_warmed(&mut self, _ctx: &mut NativeCtx<'_>, warmed: Warmed) {
        self.0.observed.lock().expect("observed").warmed.push(warmed);
    }

    #[handler::single]
    fn on_probe(&mut self, _ctx: &mut NativeCtx<'_>, _probe: Probe) -> Counts {
        let observed = self.0.observed.lock().expect("observed");
        Counts {
            reads: observed.reads.len().try_into().expect("read count"),
            outcomes: observed.outcomes.len().try_into().expect("outcome count"),
            invocations: observed.invocations.len().try_into().expect("invocation count"),
            warmed: observed.warmed.len().try_into().expect("warm count"),
        }
    }
}

struct ImpostorPeer(Shared);

#[actor(root)]
impl NativeActor for ImpostorPeer {
    type Config = ();
    type Params = Shared;
    const NAMESPACE: &'static str = "test.resolver.impostor";

    fn init((): (), shared: Shared, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self(shared))
    }

    #[handler::manual]
    fn on_respond(&mut self, ctx: &mut NativeCtx<'_, aether_actor::Erased, aether_actor::Manual>, command: Respond) {
        respond(&self.0, ctx, command);
    }
}

fn reply<R: Kind>(harness: &mut SubstrateHarness, target: ErasedActorRef, mail: &impl Kind) -> R {
    harness
        .execute(vec![("reply", HarnessOp::send_and_await_reply(target, mail))])
        .expect("execute")
        .reply::<R>("reply")
        .expect("reply")
}

fn wait_for(
    harness: &mut SubstrateHarness,
    driver: ErasedActorRef,
    reads: u32,
    outcomes: u32,
    invocations: u32,
    warmed: u32,
) {
    harness
        .execute(vec![(
            "counts",
            HarnessOp::poll_until(driver, &Probe, move |counts: &Counts| {
                counts.reads == reads
                    && counts.outcomes == outcomes
                    && counts.invocations == invocations
                    && counts.warmed == warmed
            }),
        )])
        .expect("counts reached");
}

#[test]
fn pending_folds_authenticate_replies() -> Result<(), Box<dyn Error>> {
    run(false)
}

#[test]
fn pending_fold_cancels_on_teardown() -> Result<(), Box<dyn Error>> {
    run(true)
}

#[test]
fn replay_agrees_across_warm_batch_boundaries() -> Result<(), Box<dyn Error>> {
    let Some(one_batch) = replay_outcome(false)? else {
        return Ok(());
    };
    let split_batches = replay_outcome(true)?.expect("fixture availability is stable");

    assert_eq!(one_batch, split_batches);
    assert!(matches!(one_batch, Evaluated::Completed { seq: 3, ref intents } if intents.len() == 1));
    Ok(())
}

#[test]
fn equal_digest_program_and_view_reads_keep_their_routes() -> Result<(), Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let value = EncodedArtifact::new(&ResolverValue { value: 42 })?;
    let receipt = EncodedArtifact::new(&ResolverReceipt { value: Ref::from_digest(value.digest()), marker: 7 })?;
    let input = EncodedArtifact::new(&SummarizeInput { text: Ref::<Utf8Text>::from_digest(value.digest()) })?;
    let shared = Shared {
        root: Arc::new(OnceLock::new()),
        observed: Arc::new(Mutex::new(Observed::default())),
        artifacts: Arc::new(vec![
            ClosureArtifact::new(value.kind(), value.bytes().to_vec()),
            ClosureArtifact::new(receipt.kind(), receipt.bytes().to_vec()),
        ]),
        event: resolver_event(1, receipt.digest())?,
        invoke: Some(Arc::new(Invoke::new(
            9,
            ProgramName::new(ASYNC_SUMMARIZE_PROGRAM)?,
            input.digest(),
            vec![ClosureArtifact::new(input.kind(), input.bytes().to_vec())],
        ))),
        warms: Arc::new(Vec::new()),
    };
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<DriverPeer>(shared.clone())
        .build()?;
    let driver = harness.actor_ref::<DriverPeer>().erase();
    let (root, path) = load_root(&mut harness, fs::read(wasm_path)?, "equal-digest")?;
    shared.root.set(root).expect("set root");

    let _: Ack = reply(&mut harness, driver, &Start);
    wait_for(&mut harness, driver, 1, 0, 0, 0);
    let _: Ack = reply(&mut harness, driver, &Respond { request: 0, correlation_offset: 0 });
    wait_for(&mut harness, driver, 2, 0, 0, 0);
    let _: Ack = reply(&mut harness, driver, &StartInvoke);
    wait_for(&mut harness, driver, 3, 0, 0, 0);
    {
        let observed = shared.observed.lock().expect("observed");
        assert_eq!(observed.reads[1].1.digest, observed.reads[2].1.digest);
        drop(observed);
    }

    let _: Ack = reply(&mut harness, driver, &Respond { request: 2, correlation_offset: 0 });
    wait_for(&mut harness, driver, 3, 0, 1, 0);
    assert!(matches!(&shared.observed.lock().expect("observed").invocations[0], Invoked::Refused { seq: 9, .. }));

    let _: Ack = reply(&mut harness, driver, &Respond { request: 1, correlation_offset: 0 });
    wait_for(&mut harness, driver, 3, 1, 1, 0);
    assert!(matches!(
        &shared.observed.lock().expect("observed").outcomes[0],
        Evaluated::Completed { seq: 1, intents } if intents.len() == 1
    ));

    let host = harness.actor_ref::<ComponentHostCapability>().erase();
    assert!(matches!(reply::<DropResult>(&mut harness, host, &DropComponent { target: path }), DropResult::Ok));
    Ok(())
}

fn load_root(
    harness: &mut SubstrateHarness,
    wasm: Vec<u8>,
    name: &str,
) -> Result<(ErasedActorRef, aether_data::ErasedActorPath), Box<dyn Error>> {
    Ok(harness.load_any(&LoadComponent {
        wasm,
        name: Some(name.into()),
        config: Vec::new(),
        export: Some(BUNDLE_NAMESPACE.into()),
    })?)
}

fn resolver_event(seq: u64, receipt: aether_bloomery_kinds::Digest) -> Result<Event, Box<dyn Error>> {
    Ok(Event::new(resolver_entry(seq, receipt)?))
}

fn resolver_entry(seq: u64, receipt: aether_bloomery_kinds::Digest) -> Result<JournalEntry, Box<dyn Error>> {
    let moved = RESOLVER_INPUT.move_to(Ref::from_digest(receipt));
    Ok(JournalEntry {
        seq,
        kind: HeadMoved::<ResolverReceipt>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<ResolverReceipt>::encode_storage(&StorageData::from_value(moved))?,
    })
}

fn replay_outcome(split: bool) -> Result<Option<Evaluated>, Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(None);
    };
    let value = EncodedArtifact::new(&ResolverValue { value: 42 })?;
    let receipts = [
        EncodedArtifact::new(&ResolverReceipt { value: Ref::from_digest(value.digest()), marker: 1 })?,
        EncodedArtifact::new(&ResolverReceipt { value: Ref::from_digest(value.digest()), marker: 2 })?,
        EncodedArtifact::new(&ResolverReceipt { value: Ref::from_digest(value.digest()), marker: 3 })?,
    ];
    let entries = [resolver_entry(1, receipts[0].digest())?, resolver_entry(2, receipts[1].digest())?];
    let warms = if split {
        vec![
            Warm::new(WarmEntries::new(vec![entries[0].clone()])?),
            Warm::new(WarmEntries::new(vec![entries[1].clone()])?),
        ]
    } else {
        vec![Warm::new(WarmEntries::new(entries.to_vec())?)]
    };
    let shared = Shared {
        root: Arc::new(OnceLock::new()),
        observed: Arc::new(Mutex::new(Observed::default())),
        artifacts: Arc::new(vec![
            ClosureArtifact::new(value.kind(), value.bytes().to_vec()),
            ClosureArtifact::new(receipts[0].kind(), receipts[0].bytes().to_vec()),
            ClosureArtifact::new(receipts[1].kind(), receipts[1].bytes().to_vec()),
            ClosureArtifact::new(receipts[2].kind(), receipts[2].bytes().to_vec()),
        ]),
        event: resolver_event(3, receipts[2].digest())?,
        invoke: None,
        warms: Arc::new(warms),
    };
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<DriverPeer>(shared.clone())
        .build()?;
    let driver = harness.actor_ref::<DriverPeer>().erase();
    let (root, path) = load_root(&mut harness, fs::read(wasm_path)?, "replay")?;
    shared.root.set(root).expect("set root");
    let mut reads = 0;
    for batch in 0..shared.warms.len() {
        let _: Ack = reply(&mut harness, driver, &StartWarm { batch: batch.try_into()? });
        let entry_count = if split {
            1
        } else {
            2
        };
        for _ in 0..entry_count * 2 {
            reads += 1;
            wait_for(&mut harness, driver, reads, 0, 0, batch.try_into()?);
            let _: Ack = reply(&mut harness, driver, &Respond { request: reads - 1, correlation_offset: 0 });
        }
        wait_for(&mut harness, driver, reads, 0, 0, (batch + 1).try_into()?);
    }
    {
        let observed = shared.observed.lock().expect("observed");
        if split {
            assert_eq!(observed.warmed, [Warmed::Folded { through: 1 }, Warmed::Folded { through: 2 }]);
        } else {
            assert_eq!(observed.warmed, [Warmed::Folded { through: 2 }]);
        }
        drop(observed);
    }

    let _: Ack = reply(&mut harness, driver, &Start);
    wait_for(&mut harness, driver, 5, 0, 0, shared.warms.len().try_into()?);
    let _: Ack = reply(&mut harness, driver, &Respond { request: 4, correlation_offset: 0 });
    wait_for(&mut harness, driver, 6, 0, 0, shared.warms.len().try_into()?);
    let _: Ack = reply(&mut harness, driver, &Respond { request: 5, correlation_offset: 0 });
    wait_for(&mut harness, driver, 6, 1, 0, shared.warms.len().try_into()?);
    let outcome = shared.observed.lock().expect("observed").outcomes.pop().expect("one outcome");

    let host = harness.actor_ref::<ComponentHostCapability>().erase();
    assert!(matches!(reply::<DropResult>(&mut harness, host, &DropComponent { target: path }), DropResult::Ok));
    Ok(Some(outcome))
}

fn run(cancel: bool) -> Result<(), Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let value = EncodedArtifact::new(&ResolverValue { value: 42 })?;
    let receipt = EncodedArtifact::new(&ResolverReceipt { value: Ref::from_digest(value.digest()), marker: 7 })?;
    let shared = Shared {
        root: Arc::new(OnceLock::new()),
        observed: Arc::new(Mutex::new(Observed::default())),
        artifacts: Arc::new(vec![
            ClosureArtifact::new(value.kind(), value.bytes().to_vec()),
            ClosureArtifact::new(receipt.kind(), receipt.bytes().to_vec()),
        ]),
        event: resolver_event(1, receipt.digest())?,
        invoke: None,
        warms: Arc::new(Vec::new()),
    };
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<DriverPeer>(shared.clone())
        .with_actor::<ImpostorPeer>(shared.clone())
        .build()?;
    let driver = harness.actor_ref::<DriverPeer>().erase();
    let impostor = harness.actor_ref::<ImpostorPeer>().erase();
    let (root, path) = load_root(&mut harness, fs::read(wasm_path)?, "resolver-routing")?;
    shared.root.set(root).expect("set root");
    let _: Ack = reply(&mut harness, driver, &Start);
    wait_for(&mut harness, driver, 1, 0, 0, 0);
    let host = harness.actor_ref::<ComponentHostCapability>().erase();
    if cancel {
        assert!(matches!(reply::<DropResult>(&mut harness, host, &DropComponent { target: path }), DropResult::Ok));
        let _: Ack = reply(&mut harness, driver, &Respond { request: 0, correlation_offset: 0 });
        let counts = reply::<Counts>(&mut harness, driver, &Probe);
        assert_eq!(counts.reads, 1);
        assert_eq!(counts.outcomes, 0);
        assert_eq!(counts.invocations, 0);
        assert_eq!(counts.warmed, 0);
        return Ok(());
    }

    // Neither a genuine correlation from an impostor nor an unknown correlation
    // from the real driver may consume the pending route.
    let _: Ack = reply(&mut harness, impostor, &Respond { request: 0, correlation_offset: 0 });
    let _: Ack = reply(&mut harness, driver, &Respond { request: 0, correlation_offset: 1000 });
    assert_eq!(reply::<Status>(&mut harness, root, &StatusQuery), Status::new(0, false));
    let _: Ack = reply(&mut harness, driver, &Respond { request: 0, correlation_offset: 0 });
    wait_for(&mut harness, driver, 2, 0, 0, 0);

    // A duplicate receipt cannot answer the nested value read.
    let _: Ack = reply(&mut harness, driver, &Respond { request: 0, correlation_offset: 0 });
    assert_eq!(reply::<Status>(&mut harness, root, &StatusQuery), Status::new(0, false));
    let _: Ack = reply(&mut harness, driver, &Respond { request: 1, correlation_offset: 0 });
    wait_for(&mut harness, driver, 2, 1, 0, 0);
    assert_eq!(reply::<Status>(&mut harness, root, &StatusQuery), Status::new(1, false));
    assert!(
        matches!(&shared.observed.lock().expect("observed").outcomes[0], Evaluated::Completed { seq: 1, intents } if intents.len() == 1)
    );
    let _: Ack = reply(&mut harness, driver, &Respond { request: 1, correlation_offset: 0 });
    assert_eq!(reply::<Status>(&mut harness, root, &StatusQuery), Status::new(1, false));

    assert!(matches!(reply::<DropResult>(&mut harness, host, &DropComponent { target: path }), DropResult::Ok));
    Ok(())
}
