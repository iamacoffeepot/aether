//! End-to-end: one `muse.turn` Sampled call on the shipped bloomery composition, answered by a loopback stub server
//! when the harness allows its host, recorded as a refused turn when the deny-by-default allowlist holds, and
//! refused before dialing when the binary's own flags bind an engine secret to a host the turn reaches over plain
//! http. The driver then declares the loaded bundle's programs with the schemas its records carry.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::thread;

use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, Detail, Digest, Fault, FaultReason, Head, NativeOrigin, OpaqueBytes, ProgramName, ProgramRef,
    RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, artifact_digest,
};
use aether_bloomery_muse::{
    Endpoint, InputLimit, ModelName, OfferedTools, OutputBudget, ReasoningEffort, Role, Session, TurnInput, TurnItem,
    TurnItems, TurnOutcome, TurnResult,
};
use aether_chassis_bloomery::BloomeryCli;
use aether_data::{Kind, Schema, SchemaType, wire};
use aether_harness_bloomery::{BloomeryHarness, Record, SeededJournal, StubVendor};
use aether_harness_substrate::test_helpers::require_wasm;
use clap::Parser;

/// The recorded responses-API reply the stub server sends.
const COMPLETED: &[u8] = include_bytes!("../../aether-bloomery-muse/fixtures/completed.json");

/// The `muse` head the seed binds to the bundle.
const MUSE: Head<OpaqueBytes> = Head::new("muse");

/// The conversation the turn resends, in order.
const ITEMS: [(Role, &str); 2] = [(Role::Developer, "Be brief."), (Role::User, "What is a bloomery?")];

/// The obviously fake secret value the bound-secret scenario binds. No record may carry it.
const FAKE_SECRET: &str = "fake-muse-value-for-test";

/// A seed holding the `muse` bundle under [`MUSE`], the conversation's texts, and a turn input posting to one
/// endpoint.
struct MuseSeed {
    batch: Batch,
    bundle: Digest,
    input: Digest,
}

/// Seed the bundle and a turn posting to `endpoint`, or `None` when the bundle wasm is not built.
fn seed(endpoint: &str) -> Result<Option<MuseSeed>, Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_bloomery_muse") else {
        return Ok(None);
    };
    let wasm = fs::read(&wasm_path)?;
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&wasm).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MUSE), bundle), None)?;

    let items = ITEMS.iter().map(|&(role, text)| TurnItem::message(role, batch.stage_text(text))).collect();
    let input = TurnInput::new(
        Endpoint::new(endpoint)?,
        ModelName::new("muse-spark-1.3")?,
        OfferedTools::default(),
        TurnItems::new(items)?,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
        InputLimit::new(u64::MAX).expect("limit"),
    );
    let input = batch.stage_encoded(&input)?.digest();
    Ok(Some(MuseSeed { batch, bundle, input }))
}

/// The one `muse.turn` call each scenario makes, and the `Requested` it records.
fn turn(seed: &MuseSeed) -> Result<(Call, Requested), Box<dyn Error>> {
    let origin = NativeOrigin::new("test.muse")?;
    let name = ProgramName::new("muse.turn")?;
    let call = Call { program: MUSE, name: name.clone(), input: seed.input, origin: origin.clone(), key: 1 };
    let requested = Requested {
        program: ProgramRef::new(seed.bundle, name),
        input: seed.input,
        source: RequestSource::Native { origin, key: 1 },
    };
    Ok((call, requested))
}

/// Run `scenario` against a loopback stub vendor that answers every request with [`COMPLETED`].
fn with_vendor(scenario: impl FnOnce(&StubVendor<'_>) -> Result<(), Box<dyn Error>>) -> Result<(), Box<dyn Error>> {
    thread::scope(|scope| scenario(&StubVendor::start(scope, |_| COMPLETED.to_vec())?))
}

#[test]
fn a_muse_turn_records_the_reply_a_loopback_server_sends() -> Result<(), Box<dyn Error>> {
    // Catches HTTP not composed (the call is never answered), the harness allowlist not reaching the capability,
    // the request body or method lost between program and wire, and the reply body not staged or cited.
    with_vendor(|vendor| {
        let Some(seed) = seed(&vendor.endpoint())? else {
            return Ok(());
        };
        let (call, requested) = turn(&seed)?;
        let mut harness = BloomeryHarness::start_allowing([seed.batch], ["127.0.0.1"]);

        let outcome = harness.call(&call);
        let CallOutcome::Transition { key: 1, seq: 3, transition } = outcome else {
            panic!("expected the turn's Transition at seq 3, got {outcome:?}");
        };
        harness.assert_appended(
            Seq(1),
            &[Record::equal(None, requested), Record::equal(Some(Seq(2)), transition.clone())],
        );

        let result = JournalReader::open(harness.journal_path())?
            .get::<TurnResult>(&transition.result)?
            .expect("the transition cites a stored turn result");
        assert_eq!(result.status().expect("a received turn").get(), 200);
        let TurnOutcome::Completed { text, .. } = result.outcome() else {
            panic!("expected a Completed turn, got {:?}", result.outcome());
        };
        assert_eq!(*text, Ref::of_text("A bloomery is a furnace that smelts iron into a bloom."));
        assert!(harness.stores(&text.digest()), "the answer text is stored");
        let body = result.body().expect("a received turn");
        assert_eq!(body.digest(), artifact_digest(OpaqueBytes::ID, COMPLETED), "the raw reply body is kept");
        assert!(harness.stores(&body.digest()), "the raw reply body is stored");

        let served = vendor.served();
        let [served] = served.as_slice() else {
            panic!("the turn sends exactly one request, sent {}", served.len());
        };
        assert!(served.head.starts_with("POST /v1/responses HTTP/1.1\r\n"), "request line: {}", served.head);
        let body: serde_json::Value = serde_json::from_slice(&served.body)?;
        assert_eq!(body["store"], false, "the turn is stateless on the vendor side");
        let sent: Vec<_> = body["input"]
            .as_array()
            .expect("the body carries the conversation")
            .iter()
            .map(|item| (item["role"].as_str(), item["content"][0]["text"].as_str()))
            .collect();
        assert_eq!(sent, [(Some("developer"), Some("Be brief.")), (Some("user"), Some("What is a bloomery?"))]);
        Ok(())
    })
}

#[test]
fn an_empty_allowlist_records_a_refused_turn_without_dialing() -> Result<(), Box<dyn Error>> {
    // Catches the silent hang (without `aether.http` the fetch warn-drops and the call is never answered, so the
    // harness panics at its thirty-second bound), egress open by default, and hermetic resolution not in effect.
    with_vendor(|vendor| {
        let Some(seed) = seed(&vendor.endpoint())? else {
            return Ok(());
        };
        let (call, requested) = turn(&seed)?;
        let mut harness = BloomeryHarness::start([seed.batch]);

        let outcome = harness.call(&call);
        let CallOutcome::Fault { key: 1, seq: 3, fault } = outcome else {
            panic!("expected the turn's Fault at seq 3, got {outcome:?}");
        };
        assert_eq!(fault.reason, FaultReason::Refused { reason: Detail::new("AllowlistDenied") });
        harness.assert_appended(
            Seq(1),
            &[
                Record::equal(None, requested.clone()),
                Record::equal(
                    Some(Seq(2)),
                    Fault { program: requested.program, input: requested.input, reason: fault.reason },
                ),
            ],
        );
        assert!(vendor.served().is_empty(), "a denied fetch sends no request");
        Ok(())
    })
}

#[test]
fn the_driver_declares_a_loaded_bundle_s_programs_with_their_schemas() -> Result<(), Box<dyn Error>> {
    // Catches a bundle generator that fills a record from the wrong type, labels lost between the real wasm and the
    // driver (the schema decodes without its field names), and a driver that answers from no table.
    with_vendor(|vendor| {
        let Some(seed) = seed(&vendor.endpoint())? else {
            return Ok(());
        };
        let (call, _) = turn(&seed)?;
        let mut harness = BloomeryHarness::start([seed.batch]);
        let outcome = harness.call(&call);
        assert!(matches!(outcome, CallOutcome::Fault { .. }), "expected the denied turn's Fault, got {outcome:?}");

        let declarations = harness.declarations();
        let muse = declarations
            .bundles
            .iter()
            .find(|declared| declared.bundle == seed.bundle)
            .expect("the driver declares the loaded muse bundle");
        let record = muse
            .programs
            .iter()
            .find(|program| program.name.as_str() == "muse.session.record")
            .expect("the muse bundle declares muse.session.record");
        assert_eq!(record.result_name, "muse.session");
        let schema = wire::from_bytes::<SchemaType>(&record.result_schema).expect("the result schema decodes");
        assert_eq!(schema, <Session as Schema>::SCHEMA);
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn a_bound_secret_refuses_a_plain_http_turn_without_dialing() -> Result<(), Box<dyn Error>> {
    // Catches the binary's `--secrets-dir` / `--http-secrets` flags not reaching the bloomery's `aether.http` (the
    // turn dials the stub, or is refused `AllowlistDenied`), a bound secret sent over cleartext, and the value
    // leaking into the recorded fault or anywhere else in the journal.
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    with_vendor(|vendor| {
        let Some(seed) = seed(&vendor.endpoint())? else {
            return Ok(());
        };
        let (call, requested) = turn(&seed)?;
        let journal = SeededJournal::new([seed.batch]);
        let scratch = journal.journal_path().parent().expect("the seed journal sits in a directory").to_path_buf();
        let secrets = scratch.join("secrets");
        fs::create_dir(&secrets)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(secrets.join("muse"))?
            .write_all(FAKE_SECRET.as_bytes())?;

        let secrets_dir = secrets.display().to_string();
        let cli = BloomeryCli::try_parse_from([
            "aether-bloomery",
            "--secrets-dir",
            &secrets_dir,
            "--http-allowlist",
            "127.0.0.1",
            "--http-secrets",
            "127.0.0.1/bearer=muse",
        ])?;
        let mut harness = journal.boot_with_argv(cli);

        let outcome = harness.call(&call);
        let CallOutcome::Fault { key: 1, seq: 3, fault } = outcome else {
            panic!("expected the turn's Fault at seq 3, got {outcome:?}");
        };
        let FaultReason::Refused { reason } = &fault.reason else {
            panic!("expected a refused turn, got {:?}", fault.reason);
        };
        assert!(!reason.as_str().contains(FAKE_SECRET), "the recorded fault never carries the secret value");
        assert_eq!(
            reason.as_str(),
            concat!(
                r#"InvalidUrl("plain http to 127.0.0.1 refused: "#,
                r#"a secret is bound to this host, and secrets travel only over https")"#,
            ),
        );
        harness.assert_appended(
            Seq(1),
            &[
                Record::equal(None, requested.clone()),
                Record::equal(
                    Some(Seq(2)),
                    Fault { program: requested.program, input: requested.input, reason: fault.reason.clone() },
                ),
            ],
        );
        assert!(vendor.served().is_empty(), "a refused fetch sends no request");

        // Every file under the journal root, `blobs/` included: an artifact's bytes are a blob file, not a row.
        let mut scanned = 0;
        let mut pending = vec![harness.journal_path().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let bytes = fs::read(&path)?;
                let leaked = bytes.windows(FAKE_SECRET.len()).any(|window| window == FAKE_SECRET.as_bytes());
                assert!(!leaked, "{} carries the secret value", path.display());
                scanned += 1;
            }
        }
        assert!(scanned > 1, "the scan reached the journal's log and its blob files");
        Ok(())
    })
}
