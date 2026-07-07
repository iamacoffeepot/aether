// A behavior with a `&mut K` intercept, a `&K` observe, an `#[on_attach]`,
// and a derived-serde state struct. The generated dispatch table, exports
// manifest, and `Behavior` impl must typecheck.

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::{behavior, on, on_attach};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.slider")]
struct Slider {
    value: u32,
}

#[derive(Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.focus")]
struct Focus {
    focused: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Clamp {
    seen: u32,
}

#[behavior]
impl Behavior for Clamp {
    #[on]
    fn clamp(&mut self, _ctx: &mut BehaviorCtx, slider: &mut Slider) {
        if slider.value > 200 {
            slider.value = 200;
        }
        self.seen += 1;
    }

    #[on]
    fn watch(&mut self, ctx: &mut BehaviorCtx, focus: &Focus) {
        if focus.focused {
            ctx.widget().set(&Slider { value: 0 });
        }
    }

    #[on_attach]
    fn setup(&mut self, _ctx: &mut BehaviorCtx) {
        self.seen = 0;
    }
}

fn main() {
    // Touch the generated manifest const so it is not stripped, confirming
    // it typechecks against the exports section length.
    assert_eq!(
        Clamp::__AETHER_BEHAVIOR_EXPORTS_LEN,
        Clamp::__AETHER_BEHAVIOR_EXPORTS.len()
    );

    let clamp = Clamp::default();
    let _ = clamp.state_save();
}
