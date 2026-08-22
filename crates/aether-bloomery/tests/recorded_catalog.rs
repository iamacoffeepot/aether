//! The seal records the catalog it resolved so the fold reads the record
//! (#4944), and pre-existing rows without that effect keep the compiled-line
//! fallback.

mod common;

use aether_bloomery::{
    Decision, Decisions, Fact, Outcome, ResolvedConfigs, Snapshot, SpendWindow, StageCatalog, StageId, reduce,
};
use common::{draft, draft_with_catalog, event, membership};

#[test]
fn a_newly_sealed_bloom_records_the_catalog_admission_resolved() {
    // The fold must not re-derive the catalog from the compiled line: a later
    // binary with an edited line would rewrite a bloom that sealed none.
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let decisions = reduce(
        &Snapshot::new(common::digest(1)).with_green_base(common::digest(1)),
        &event("seal", Fact::Seal(spec)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    match decisions.effects.iter().find(|effect| matches!(effect, Decision::RecordStageCatalog { .. })) {
        Some(Decision::RecordStageCatalog { bloom: recorded, catalog }) => {
            assert_eq!(*recorded, bloom);
            assert_eq!(*catalog, StageCatalog::line());
        }
        other => panic!("seal must record the catalog it resolved, got {other:?}"),
    }
}

#[test]
fn the_fold_reads_the_recorded_catalog_not_a_re_resolution() {
    // Plausible bug: apply keeps calling sealed_in and ignores the recorded
    // effect, so a no-catalog bloom still tracks the compiled line.
    let mut catalog = StageCatalog::line();
    catalog
        .bindings
        .iter_mut()
        .find(|binding| binding.stage == StageId::Construct)
        .expect("compiled line binds Construct")
        .retry_budget = 99;

    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let decisions = Decisions {
        outcome: Outcome::Sealed(bloom),
        effects: vec![Decision::RecordStageCatalog { bloom, catalog: catalog.clone() }],
    };
    let snapshot = Snapshot::new(common::digest(1)).with_green_base(common::digest(1)).apply(
        &event("seal", Fact::Seal(spec)),
        &decisions,
        &ResolvedConfigs::default(),
    );
    assert_eq!(
        snapshot.blooms.get(&bloom).expect("sealed").stage_catalog,
        catalog,
        "the fold must copy the recorded catalog, not re-resolve"
    );
}

#[test]
fn a_sealed_catalog_is_what_the_effect_carries() {
    let mut catalog = StageCatalog::line();
    catalog
        .bindings
        .iter_mut()
        .find(|binding| binding.stage == StageId::Verify)
        .expect("compiled line binds Verify")
        .retry_budget = 7;
    let (draft, configs) = draft_with_catalog(1, vec![membership("alpha", 10)], &catalog);
    let spec = draft.seal();
    let bloom = spec.id();
    let decisions = reduce(
        &Snapshot::new(common::digest(1)).with_green_base(common::digest(1)),
        &event("seal", Fact::Seal(spec.clone())),
        &configs,
        &SpendWindow::default(),
    );
    match decisions.effects.iter().find(|effect| matches!(effect, Decision::RecordStageCatalog { .. })) {
        Some(Decision::RecordStageCatalog { catalog: recorded, .. }) => {
            assert_eq!(*recorded, catalog);
        }
        other => panic!("expected RecordStageCatalog, got {other:?}"),
    }
    let snapshot = Snapshot::new(common::digest(1)).with_green_base(common::digest(1)).apply(
        &event("seal", Fact::Seal(spec)),
        &decisions,
        &configs,
    );
    assert_eq!(snapshot.blooms.get(&bloom).expect("sealed").stage_catalog, catalog);
}

#[test]
fn pre_existing_rows_without_a_recorded_catalog_keep_the_compiled_line_fallback() {
    // Stated where it happens: BloomRecord::sealed falls back through
    // StageCatalog::sealed_in, which is the compiled line when the spec sealed
    // none. A journal written before RecordStageCatalog must still fold.
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let decisions = Decisions { outcome: Outcome::Sealed(bloom), effects: Vec::new() };
    let snapshot = Snapshot::new(common::digest(1)).with_green_base(common::digest(1)).apply(
        &event("seal", Fact::Seal(spec)),
        &decisions,
        &ResolvedConfigs::default(),
    );
    assert_eq!(
        snapshot.blooms.get(&bloom).expect("sealed").stage_catalog,
        StageCatalog::line(),
        "pre-RecordStageCatalog rows keep the compiled-line fallback"
    );
}
