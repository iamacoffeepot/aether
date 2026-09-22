//! Boot the bloomery chassis for real: build [`BloomeryChassis`] over a journal
//! path, then observe `Processed` through the mounted driver.
//!
//! Each test builds [`BloomeryEnv`] directly, with default base members, and
//! calls [`BloomeryChassis::build`] without ever calling `run()`, so the
//! passives tear down on drop.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use aether_bloomery_driver::BundleDriver;
use aether_bloomery_journal::{Batch, Journal};
use aether_bloomery_kinds::{
    AwaitProcessed, ClosureLimit, Head, Processed, RecordedHead, RecordedHeadMove, Seq, Utf8Text,
};
use aether_chassis::boot::{ChassisBase, RuntimeConfig};
use aether_chassis_bloomery::BloomeryConfig;
use aether_chassis_bloomery::chassis::{BloomeryChassis, BloomeryEnv};
use aether_data::MailboxId;
use aether_substrate::Chassis;
use aether_substrate::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigSources;

/// Await `Processed` through the mounted driver: resolve the driver's born id,
/// spawn a probe that sends one `AwaitProcessed`, and wait thirty seconds.
fn await_processed(built: &BuiltChassis<BloomeryChassis>, through: u64) -> Processed {
    let driver =
        built.resolve_address("aether.bloomery.driver:driver").expect("the driver is spawned at mount").mailbox_id;
    let (sink, rx) = mpsc::channel();
    built
        .spawn_actor::<Probe>(Subname::Named("probe"), (), ProbeParams { driver, through, sink })
        .finish()
        .expect("probe birth");
    rx.recv_timeout(Duration::from_secs(30)).expect("Processed within thirty seconds")
}

/// A chassis env with default base members and the bloomery knobs pointing at
/// `journal`, or unset when `None`.
fn test_env(journal: Option<String>) -> BloomeryEnv {
    BloomeryEnv {
        base: ChassisBase { sources: ConfigSources::new(None), ..Default::default() },
        runtime: RuntimeConfig::default(),
        bloomery: BloomeryConfig { journal, closure_limit_bytes: ClosureLimit::MAX_BYTES },
    }
}

/// Seed one committed entry: one staged text under one head move, the shape of
/// `seed_barrier_journal` in the driver's reactor tests.
fn seed_single_entry_journal(path: &Path) {
    let mut journal = Journal::open(path).expect("open the seed journal");
    let mut batch = Batch::new();
    let staged = batch.stage_text("one");
    let head: Head<Utf8Text> = Head::new("test.bloomery.chassis.first");
    batch
        .push_event(&RecordedHeadMove::new(RecordedHead::from(&head), staged.digest()), None)
        .expect("stage the head move");
    journal.append(Seq(0), &batch).expect("append the seed batch");
}

/// Test-local probe: sends one `AwaitProcessed` at `wire` and forwards the
/// `Processed` reply to the observer sink.
struct Probe {
    driver: MailboxId,
    through: u64,
    sink: mpsc::Sender<Processed>,
}

struct ProbeParams {
    driver: MailboxId,
    through: u64,
    sink: mpsc::Sender<Processed>,
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for Probe {
    type Config = ();
    type Params = ProbeParams;
    const NAMESPACE: &'static str = "test.bloomery.chassis.probe";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { driver: params.driver, through: params.through, sink: params.sink })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        ctx.actor_at::<BundleDriver>(self.driver).send(&AwaitProcessed { through: self.through });
    }

    #[aether_actor::handler::single]
    fn on_processed(&mut self, _ctx: &mut NativeCtx<'_>, processed: Processed) {
        let _ = self.sink.send(processed);
    }
}

#[test]
fn fresh_journal_boots_and_quiesces_at_zero() {
    // Catches three bugs: a driver not spawned or not wired to the journal id
    // (its startup reads warn-drop and `check_processed` never sees
    // `routing.started`), a chassis that fails on a first-run absent file, and
    // a journal opened somewhere other than the configured path.
    let temp = tempfile::tempdir().expect("a scratch dir for the journal");
    let path = temp.path().join("journal.sqlite");
    assert!(!path.exists(), "the journal file must not exist before boot");
    let built = BloomeryChassis::build(test_env(Some(path.display().to_string()))).expect("boot over a fresh journal");
    assert_eq!(await_processed(&built, 0).head, 0);
    assert!(path.exists(), "boot creates the journal file at the configured path");
}

#[test]
fn populated_journal_is_recovered_through_its_head() {
    // Catches the configured path being ignored or reinterpreted: an empty
    // journal would never route through 1 and would answer no `Processed`.
    let temp = tempfile::tempdir().expect("a scratch dir for the journal");
    let path = temp.path().join("journal.sqlite");
    seed_single_entry_journal(&path);
    let built =
        BloomeryChassis::build(test_env(Some(path.display().to_string()))).expect("boot over a populated journal");
    assert_eq!(await_processed(&built, 1).head, 1);
}

#[test]
fn unset_journal_refuses_boot() {
    // Catches a silent default, for example `PathBuf::new()`: rusqlite opens
    // `""` as a throwaway temporary database, which silently loses every write.
    let error = BloomeryChassis::build(test_env(None)).expect_err("boot without a journal path must fail");
    let message = error.to_string();
    assert!(message.contains("AETHER_BLOOMERY_JOURNAL"), "the refusal names the env key: {message}");
}
