use alloc::string::String;

use aether_data::Kind;
use serde::{Deserialize, Serialize};

use super::{BehaviorCtx, MirrorStore, run_filter};
use crate::envelope::{EffectTarget, Verdict};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.slider")]
struct Slider {
    value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.label")]
struct Label {
    text: String,
}

// Exercises the effect accumulator + verdict drain: the three handle verbs
// each push one targeted effect, `consume` sets the verdict, and the drained
// `FilterOutput` reports them verdict-first, effects in recorded order, with
// the right targets / kind ids / bytes. All owned SDK logic.
#[test]
fn drain_reports_verdict_then_ordered_effects() {
    let inbound = Slider { value: 5 }.encode_into_bytes();
    let mut mirrors = MirrorStore::default();
    let mut ctx = BehaviorCtx::__new_inbound(&mut mirrors, Slider::ID, &inbound);

    ctx.widget().set(&Slider { value: 8 });
    ctx.child("row/label").send(&Label { text: String::from("hi") });
    ctx.panel().emit(&Slider { value: 1 });
    ctx.consume();

    let output = ctx.__into_output();

    assert!(matches!(output.verdict, Verdict::Consume));
    assert_eq!(output.effects.len(), 3);

    assert_eq!(output.effects[0].target, EffectTarget::Widget);
    assert_eq!(output.effects[0].kind_id, Slider::ID.0);
    assert_eq!(output.effects[0].bytes, Slider { value: 8 }.encode_into_bytes());

    assert_eq!(output.effects[1].target, EffectTarget::Child(String::from("row/label")));
    assert_eq!(output.effects[1].kind_id, Label::ID.0);

    assert_eq!(output.effects[2].target, EffectTarget::Panel);
    assert_eq!(output.effects[2].kind_id, Slider::ID.0);
}

// A `&mut K`-style intercept forwards the re-encoded bytes; the default
// verdict forwards the inbound bytes unchanged.
#[test]
fn forward_carries_original_then_mutated_bytes() {
    let inbound = Slider { value: 5 }.encode_into_bytes();
    let mut original_mirrors = MirrorStore::default();

    let original = BehaviorCtx::__new_inbound(&mut original_mirrors, Slider::ID, &inbound);
    assert_eq!(original.__into_output().verdict, Verdict::Forward(inbound.clone()));

    let mut mutated_mirrors = MirrorStore::default();
    let mut mutated = BehaviorCtx::__new_inbound(&mut mutated_mirrors, Slider::ID, &inbound);
    let reencoded = Slider { value: 9 }.encode_into_bytes();
    mutated.__forward_mutated(reencoded.clone());
    assert_eq!(mutated.__into_output().verdict, Verdict::Forward(reencoded));
}

// The mirror decodes once and reflects the latest bytes: the inbound kind
// seeds the widget mirror, and an intervening own-write invalidates the
// stale decode so the next read re-decodes to the new value.
#[test]
fn mirror_decodes_inbound_and_invalidates_on_update() {
    let inbound = Slider { value: 5 }.encode_into_bytes();
    let mut mirrors = MirrorStore::default();
    let mut ctx = BehaviorCtx::__new_inbound(&mut mirrors, Slider::ID, &inbound);

    assert_eq!(ctx.widget().last::<Slider>(), Some(&Slider { value: 5 }));

    // Own write updates the mirror; the cached decode must be dropped.
    ctx.widget().set(&Slider { value: 9 });
    assert_eq!(ctx.widget().last::<Slider>(), Some(&Slider { value: 9 }));
}

#[test]
fn run_filter_preserves_mirror_across_calls() {
    let slider = Slider { value: 7 }.encode_into_bytes();
    let label = Label { text: String::from("later") }.encode_into_bytes();
    let mut mirrors = MirrorStore::default();

    let _ = run_filter(&mut mirrors, Slider::ID, &slider, |_| {});

    let mut observed = None;
    let _ = run_filter(&mut mirrors, Label::ID, &label, |ctx| {
        observed = ctx.widget().last::<Slider>().cloned();
    });

    assert_eq!(observed, Some(Slider { value: 7 }));
}
