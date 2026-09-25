//! Boot the bloomery chassis for real: build [`BloomeryChassis`] over a journal
//! root, then observe `Processed` through the mounted driver.
//!
//! The booting tests go through `aether-harness-bloomery`, which builds the
//! chassis with default base members and never calls `run()`, so the passives
//! tear down on drop. The refusal test builds [`BloomeryEnv`] directly, since
//! it asserts the boot that never produces a harness.

use aether_bloomery_journal::Batch;
use aether_bloomery_kinds::{ClosureLimit, Head, RecordedHead, RecordedHeadMove, Seq, Utf8Text};
use aether_chassis::boot::{ChassisBase, RuntimeConfig};
use aether_chassis_bloomery::BloomeryConfig;
use aether_chassis_bloomery::chassis::{BloomeryChassis, BloomeryEnv};
use aether_harness_bloomery::{BloomeryHarness, SeededJournal};
use aether_substrate::Chassis;
use aether_substrate::config::ConfigSources;

/// Seed one committed entry: one staged text under one head move, the shape of
/// `seed_barrier_batch` in the driver's reactor tests.
fn single_entry_batch() -> Batch {
    let mut batch = Batch::new();
    let staged = batch.stage_text("one");
    let head: Head<Utf8Text> = Head::new("test.bloomery.chassis.first");
    batch
        .push_event(&RecordedHeadMove::new(RecordedHead::from(&head), staged.digest()), None)
        .expect("stage the head move");
    batch
}

#[test]
fn fresh_journal_boots_and_quiesces_at_zero() {
    // Catches three bugs: a driver not spawned or not wired to the journal id
    // (its startup reads warn-drop and `check_processed` never sees
    // `routing.started`), a chassis that fails on a first-run absent root, and
    // a journal opened somewhere other than the configured root.
    let seeded = SeededJournal::new([]);
    assert!(!seeded.journal_path().exists(), "the journal root must not exist before boot");
    let mut harness = seeded.boot();
    assert_eq!(harness.settle(Seq(0)), Seq(0));
    assert!(
        harness.journal_path().join("journal.sqlite").is_file(),
        "boot creates the journal root, with its log, at the configured path"
    );
}

#[test]
fn populated_journal_is_recovered_through_its_head() {
    // Catches the configured path being ignored or reinterpreted: an empty
    // journal would never route through 1 and would answer no `Processed`.
    let mut harness = BloomeryHarness::start([single_entry_batch()]);
    assert_eq!(harness.settle(Seq(1)), Seq(1));
}

#[test]
fn unset_journal_refuses_boot() {
    // Catches a silent default, for example `PathBuf::new()`: rusqlite opens
    // `""` as a throwaway temporary database, which silently loses every write.
    let env = BloomeryEnv {
        base: ChassisBase { sources: ConfigSources::new(None), ..Default::default() },
        runtime: RuntimeConfig::default(),
        bloomery: BloomeryConfig { journal: None, closure_limit_bytes: ClosureLimit::MAX_BYTES },
    };
    let error = BloomeryChassis::build(env).expect_err("boot without a journal root must fail");
    let message = error.to_string();
    assert!(message.contains("AETHER_BLOOMERY_JOURNAL"), "the refusal names the env key: {message}");
}
