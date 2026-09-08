//! The seal records the lane vocabulary it resolved so the fold reads the
//! record (ADR-0215), and rows without that effect keep the compiled fallback.
//!
//! `recorded_catalog`'s sibling, and the same incident class one step over: a
//! recorded value that a later binary silently replaces with its own compiled
//! copy. It matters more here than it does for the catalog, because the
//! manifest came out of a *tree* — a base's `pipeline.toml` can be rewritten
//! under a digest no journal row names, so re-deriving it at replay would grade
//! a bloom against a vocabulary it never ran.

mod common;

use aether_bloomery::{
    BloomDraft, ConfigKind, ConfigRegistry, Decision, Decisions, Fact, Outcome, PipelineManifest, ResolvedConfigs,
    Snapshot, SpendWindow, reduce,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use common::{digest, draft, event, membership};

/// A vocabulary no compiled copy could produce: one identity, one lane, one
/// position. Any assertion that passes against this cannot have been answered
/// by `PipelineManifest::compiled`.
fn declared() -> PipelineManifest {
    PipelineManifest::from_toml(
        "version = 1\n\
         [entrypoint]\nprogram = \"just\"\nargs = [\"lane\"]\n\
         [lanes]\nmodel = [\"construct.implement\"]\nmechanical = []\n\
         [verifiers]\nidentities = [\"verify.fmt\"]\n\
         [verifiers.runs]\n\"verify.check\" = [\"verify.fmt\"]\n\
         [evidence]\nenvelope = 1\n",
    )
    .expect("the fixture manifest reads")
}

/// A draft whose bloom-wide registry seals `manifest`, with the
/// [`ResolvedConfigs`] that produce it — the manifest counterpart of
/// `draft_with_catalog`.
fn draft_with_manifest(manifest: &PipelineManifest) -> (BloomDraft, ResolvedConfigs) {
    let mut configs = ConfigRegistry::default();
    configs.insert::<PipelineManifest>(manifest.address());

    let mut resolved = ResolvedConfigs::default();
    resolved.insert(manifest.address(), PipelineManifest::NAME, to_vec(manifest).expect("manifest encodes"), None);

    (
        BloomDraft { proposals: vec![membership("alpha", 10)], base: digest(1), configs, ..BloomDraft::default() },
        resolved,
    )
}

fn sealed_snapshot() -> Snapshot {
    Snapshot::new(digest(1)).with_green_base(digest(1))
}

#[test]
fn a_newly_sealed_bloom_records_the_vocabulary_its_base_declared() {
    let manifest = declared();
    let (draft, configs) = draft_with_manifest(&manifest);
    let spec = draft.seal();
    let bloom = spec.id();

    let decisions =
        reduce(&sealed_snapshot(), &event("seal", Fact::Seal(spec.clone())), &configs, &SpendWindow::default());
    match decisions.effects.iter().find(|effect| matches!(effect, Decision::RecordPipelineManifest { .. })) {
        Some(Decision::RecordPipelineManifest { bloom: recorded, manifest: journaled }) => {
            assert_eq!(*recorded, bloom);
            assert_eq!(*journaled, manifest, "the seal journals the value it resolved, not the compiled one");
        }
        other => panic!("seal must record the manifest it resolved, got {other:?}"),
    }

    let snapshot = sealed_snapshot().apply(&event("seal", Fact::Seal(spec)), &decisions, &configs);
    assert_eq!(snapshot.blooms.get(&bloom).expect("sealed").pipeline_manifest, manifest);
}

#[test]
fn the_fold_reads_the_recorded_manifest_not_a_re_resolution() {
    // Plausible bug: apply keeps calling sealed_in and ignores the recorded
    // effect, so a bloom tracks whatever vocabulary the replaying binary holds
    // rather than the one it sealed. Sealing nothing and recording a declared
    // value is exactly the case a re-resolution would get wrong.
    let manifest = declared();
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let decisions = Decisions {
        outcome: Outcome::Sealed(bloom),
        effects: vec![Decision::RecordPipelineManifest { bloom, manifest: manifest.clone() }],
    };

    let snapshot = sealed_snapshot().apply(&event("seal", Fact::Seal(spec)), &decisions, &ResolvedConfigs::default());
    assert_eq!(
        snapshot.blooms.get(&bloom).expect("sealed").pipeline_manifest,
        manifest,
        "the fold must copy the recorded manifest, not re-resolve"
    );
}

#[test]
fn rows_without_a_recorded_manifest_keep_the_compiled_fallback() {
    // A journal written before this decision existed still folds: the record
    // opens on PipelineManifest::sealed_in, which is the compiled vocabulary
    // when the spec sealed none — the one that bloom actually ran under.
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let decisions = Decisions { outcome: Outcome::Sealed(bloom), effects: Vec::new() };

    let snapshot = sealed_snapshot().apply(&event("seal", Fact::Seal(spec)), &decisions, &ResolvedConfigs::default());
    assert_eq!(
        snapshot.blooms.get(&bloom).expect("sealed").pipeline_manifest,
        PipelineManifest::compiled(),
        "pre-ADR-0215 rows keep the compiled-vocabulary fallback"
    );
}
