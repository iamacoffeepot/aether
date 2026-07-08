// Native execution of the `#[behavior]` macro's generated code — the
// dispatch table's kind routing, the `&mut K` re-encode/forward, the
// consume-wins verdict interplay, the exports manifest bytes, and the
// serde `state_save`/`state_load` defaults. The trybuild pass fixture only
// proves this code compiles; these tests run it. The ctx drain mechanics
// themselves are `aether-behavior`'s own tests — what is under test here
// is the glue the macro emits around them.

use aether_behavior::envelope::Verdict;
use aether_behavior::manifest::decode_exports_manifest;
use aether_behavior::runtime::MirrorStore;
use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::behavior;
use aether_data::{Kind, KindId};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior_native.gauge")]
struct Gauge {
    value: u32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior_native.blur")]
struct Blur {
    hard: bool,
}

#[derive(Debug, PartialEq, Default, Serialize, Deserialize)]
struct Limiter {
    cap: u32,
    hits: u32,
}

#[derive(Debug, PartialEq, Default, Serialize, Deserialize)]
struct Gate {
    sealed: u32,
}

#[behavior]
impl Behavior for Limiter {
    #[on]
    fn limit(&mut self, _ctx: &mut BehaviorCtx, gauge: &mut Gauge) {
        if gauge.value > self.cap {
            gauge.value = self.cap;
        }
        self.hits += 1;
    }

    #[on]
    fn blur(&mut self, ctx: &mut BehaviorCtx, blur: &Blur) {
        if blur.hard {
            ctx.consume();
        }
        self.hits += 1;
    }

    #[on_attach]
    fn setup(&mut self, _ctx: &mut BehaviorCtx) {
        self.cap = 100;
    }
}

#[behavior]
impl Behavior for Gate {
    #[on]
    fn seal(&mut self, ctx: &mut BehaviorCtx, gauge: &mut Gauge) {
        gauge.value = 0;
        self.sealed += 1;
        ctx.consume();
    }
}

fn dispatch<K: Kind>(limiter: &mut Limiter, kind: &K) -> Verdict {
    let bytes = kind.encode_into_bytes();
    let mut mirrors = MirrorStore::default();
    let mut ctx = BehaviorCtx::__new_inbound(&mut mirrors, K::ID, &bytes);
    limiter.__aether_behavior_dispatch(&mut ctx, K::ID, &bytes);
    ctx.__into_output().verdict
}

/// A `&mut K` handler's mutation is the verdict: the forwarded bytes are
/// the re-encoded, mutated kind — not the inbound original.
#[test]
fn intercept_forwards_the_mutated_re_encode() {
    let mut limiter = Limiter { cap: 100, hits: 0 };
    let verdict = dispatch(&mut limiter, &Gauge { value: 250 });
    let Verdict::Forward(bytes) = verdict else {
        panic!("an intercept handler forwards");
    };
    assert_eq!(
        Gauge::decode_from_bytes(&bytes),
        Some(Gauge { value: 100 }),
        "the forwarded bytes carry the clamped value, not the inbound 250",
    );
    assert_eq!(limiter.hits, 1, "the intercept handler ran exactly once");
}

/// A `&K` observe handler forwards the original bytes untouched.
#[test]
fn observe_forwards_the_original_bytes() {
    let mut limiter = Limiter { cap: 100, hits: 0 };
    let inbound = Blur { hard: false };
    let verdict = dispatch(&mut limiter, &inbound);
    let Verdict::Forward(bytes) = verdict else {
        panic!("an observe handler forwards");
    };
    assert_eq!(
        bytes,
        inbound.encode_into_bytes(),
        "observe forwards the inbound encoding verbatim",
    );
    assert_eq!(limiter.hits, 1);
}

/// `ctx.consume()` drops the mail — and wins over any later re-encode.
#[test]
fn consume_drops_the_in_flight_mail() {
    let mut limiter = Limiter { cap: 100, hits: 0 };
    let verdict = dispatch(&mut limiter, &Blur { hard: true });
    assert!(
        matches!(verdict, Verdict::Consume),
        "a consumed mail must not forward",
    );
}

#[test]
fn consume_wins_over_the_intercept_re_encode() {
    let mut gate = Gate::default();
    let bytes = Gauge { value: 250 }.encode_into_bytes();
    let mut mirrors = MirrorStore::default();
    let mut ctx = BehaviorCtx::__new_inbound(&mut mirrors, Gauge::ID, &bytes);
    gate.__aether_behavior_dispatch(&mut ctx, Gauge::ID, &bytes);
    let verdict = ctx.__into_output().verdict;
    // Tripwire: the macro's unconditional __forward_mutated after an
    // intercept handler must not override a consume() verdict.
    assert!(
        matches!(verdict, Verdict::Consume),
        "consume must win over the intercept re-encode path",
    );
    assert_eq!(gate.sealed, 1, "the intercept handler body ran");
}

/// A kind with no `#[on]` handler passes through as a forward of the
/// original bytes and runs nothing — the dispatch chain's fall-through.
#[test]
fn undeclared_kind_falls_through_untouched() {
    #[derive(Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
    #[kind(name = "test.behavior_native.unhandled")]
    struct Unhandled {
        payload: u32,
    }

    let mut limiter = Limiter { cap: 100, hits: 0 };
    let inbound = Unhandled { payload: 7 };
    let verdict = dispatch(&mut limiter, &inbound);
    let Verdict::Forward(bytes) = verdict else {
        panic!("an unhandled kind forwards");
    };
    assert_eq!(bytes, inbound.encode_into_bytes());
    assert_eq!(limiter.hits, 0, "no handler ran");
}

/// The emitted exports manifest lists exactly the `#[on]` kind ids — the
/// id set the host's skip-set is built from. Lifecycle hooks ride
/// sentinels and must not appear.
#[test]
fn exports_manifest_lists_exactly_the_handled_kind_ids() {
    let mut listed: Vec<KindId> =
        decode_exports_manifest(&Limiter::__AETHER_BEHAVIOR_EXPORTS).collect();
    let mut expected = vec![Gauge::ID, Blur::ID];
    listed.sort();
    expected.sort();
    assert_eq!(listed, expected);
}

/// The macro-emitted serde `state_save`/`state_load` defaults round-trip
/// the author's struct through the wire codec.
#[test]
fn state_defaults_round_trip_the_author_struct() {
    let saved = Limiter { cap: 42, hits: 9 }.state_save();
    let mut restored = Limiter::default();
    restored.state_load(&saved);
    assert_eq!(restored, Limiter { cap: 42, hits: 9 });
}
