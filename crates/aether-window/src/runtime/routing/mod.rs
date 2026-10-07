//! Which subscribers a published window event is sent to.
//!
//! Every published kind states a [`Route`], and
//! [`WindowSubscribers::publish`](super::subscribers::WindowSubscribers::publish),
//! the one fan-out both backends and the injected path share, reads it. Most
//! kinds go to every subscriber of the kind for the window. The key and text
//! kinds are narrowed by the window's key focus slot ([`key_focus`], ADR-0248
//! §9), and two lifecycle kinds also clear routing state after they are sent.

use crate::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};

use super::subscribers::Published;
use crate::{WindowClosed, WindowFocus, WindowMenuActivated, WindowOpened};

pub(super) mod key_focus;

/// How one published event is routed among the subscribers of its kind for
/// its window.
pub(super) enum Route {
    /// To every subscriber.
    Everyone,
    /// A key going down, or repeating while down: to the subscribers its
    /// press's record admits, the record being made from the window's key
    /// focus slot when the code is not already down.
    KeyDown { code: u32 },
    /// A key going up: to the subscribers its press's record admits, and the
    /// record is removed.
    KeyUp { code: u32 },
    /// Text, committed or in composition: to the subscribers the window's key
    /// focus slot admits as it is now.
    Text,
    /// The window lost operating-system focus: to every subscriber, and the
    /// window's key records are then forgotten.
    Unfocused,
    /// The window closed: to every subscriber, and the window's key records
    /// and key focus slot are then removed.
    Closed,
}

/// A published kind's route. A kind that states none goes to every
/// subscriber.
pub(super) trait Routed: Published {
    fn route(&self) -> Route {
        Route::Everyone
    }
}

impl Routed for Key {
    fn route(&self) -> Route {
        Route::KeyDown { code: self.code }
    }
}

impl Routed for KeyRelease {
    fn route(&self) -> Route {
        Route::KeyUp { code: self.code }
    }
}

impl Routed for TextInput {
    fn route(&self) -> Route {
        Route::Text
    }
}

impl Routed for ImePreedit {
    fn route(&self) -> Route {
        Route::Text
    }
}

impl Routed for WindowFocus {
    fn route(&self) -> Route {
        if self.focused {
            Route::Everyone
        } else {
            Route::Unfocused
        }
    }
}

impl Routed for WindowClosed {
    fn route(&self) -> Route {
        Route::Closed
    }
}

impl Routed for MouseMove {}

impl Routed for MouseButton {}

impl Routed for MouseButtonRelease {}

impl Routed for MouseWheel {}

impl Routed for WindowSize {}

impl Routed for Modifiers {}

impl Routed for WindowOpened {}

impl Routed for WindowMenuActivated {}

#[cfg(all(test, feature = "synthetic"))]
mod tests {
    use std::collections::BTreeSet;

    use crate::{Key, KeyRelease, TextInput};
    use aether_actor::{ActorPath, ActorRef};
    use aether_data::{Kind, LoadName};

    use crate::KeyFocusScope::{Actor, Subtree};
    use crate::WindowSelector::All;
    use crate::runtime::subscribers::fixture::{
        Leave, Receipt, Rig, SpawnChild, Watcher, receivers, recipients, watcher, watcher_child,
    };
    use crate::{
        CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, KeyFocusGained, KeyFocusLost,
        SubscribeWindowResult, WindowCapability, WindowFocus, WindowInstance, WindowMode, WindowPresentation,
        WindowSelector, WindowSpec, WindowSubscription,
    };

    fn window(name: &str) -> ActorPath<WindowInstance> {
        WindowInstance::path(&LoadName::new(name).expect("fixture window name"))
    }

    fn key(window: &ActorPath<WindowInstance>, code: u32) -> Key {
        Key { window: window.clone(), code }
    }

    fn key_release(window: &ActorPath<WindowInstance>, code: u32) -> KeyRelease {
        KeyRelease { window: window.clone(), code }
    }

    fn gained(window: &ActorPath<WindowInstance>) -> KeyFocusGained {
        KeyFocusGained { window: window.clone() }
    }

    fn lost(window: &ActorPath<WindowInstance>) -> KeyFocusLost {
        KeyFocusLost { window: window.clone() }
    }

    /// Spawn the watcher keyed `name`, subscribed under `selector` to the
    /// three key kinds a watcher handles.
    fn key_subscriber(rig: &mut Rig<WindowCapability>, name: &str, selector: &WindowSelector) -> ActorRef<Watcher> {
        let spawned = rig.watcher(name);
        let path = watcher(name);
        for subscription in [
            WindowSubscription::Key(path.narrow()),
            WindowSubscription::KeyRelease(path.narrow()),
            WindowSubscription::TextInput(path.narrow()),
        ] {
            assert!(matches!(rig.subscribe(selector.clone(), subscription), SubscribeWindowResult::Ok));
        }
        spawned
    }

    /// Each `K` among `receipts` beside the key of the watcher that received
    /// it, sorted by that key.
    fn heard<K: Kind>(receipts: &[Receipt]) -> Vec<(&str, K)> {
        let mut heard = receipts
            .iter()
            .filter_map(|receipt| receipt.event::<K>().map(|event| (receipt.watcher.as_str(), event)))
            .collect::<Vec<_>>();
        heard.sort_unstable_by_key(|(watcher, _)| *watcher);
        heard
    }

    /// Fails if a held slot does not narrow the window's keys and text to its
    /// holder, if a release does not lift the narrowing, or if either notice
    /// is missing or names no window.
    #[test]
    fn a_take_narrows_keys_and_text_to_the_holder_until_it_releases() {
        let mut rig = Rig::synthetic();
        let console = key_subscriber(&mut rig, "console", &All);
        key_subscriber(&mut rig, "camera", &All);
        let main = window("main");

        let taken = rig.take(console, &main, Actor);
        assert_eq!(heard(&taken), [("console", gained(&main))]);
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 41))), ["console"]);
        let text = TextInput { window: main.clone(), text: "a".to_owned() };
        assert_eq!(receivers(&rig.inject(&main, &text)), ["console"]);

        let released = rig.release(console, &main);
        assert_eq!(heard(&released), [("console", lost(&main))]);
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 42))), ["camera", "console"]);
    }

    /// Fails if every window shares one slot, if a take in one window
    /// replaces the holder of another, or if a release empties every slot its
    /// sender holds.
    #[test]
    fn each_window_has_its_own_slot() {
        let mut rig = Rig::synthetic();
        let a = key_subscriber(&mut rig, "a", &All);
        let b = key_subscriber(&mut rig, "b", &All);
        let (first, second) = (window("first"), window("second"));

        rig.take(a, &first, Actor);
        assert_eq!(receivers(&rig.inject(&first, &key(&first, 1))), ["a"]);
        assert_eq!(receivers(&rig.inject(&second, &key(&second, 1))), ["a", "b"]);

        let taken = rig.take(b, &second, Actor);
        assert_eq!(heard(&taken), [("b", gained(&second))]);
        assert!(heard::<KeyFocusLost>(&taken).is_empty(), "the first window's holder keeps its slot");
        assert_eq!(receivers(&rig.inject(&first, &key(&first, 2))), ["a"]);
        assert_eq!(receivers(&rig.inject(&second, &key(&second, 2))), ["b"]);

        let taken = rig.take(a, &second, Actor);
        assert_eq!(heard(&taken), [("a", gained(&second))]);
        assert_eq!(heard(&taken), [("b", lost(&second))]);
        let released = rig.release(a, &first);
        assert_eq!(heard(&released), [("a", lost(&first))]);
        assert_eq!(receivers(&rig.inject(&second, &key(&second, 3))), ["a"], "the second window's slot stands");
        assert_eq!(receivers(&rig.inject(&first, &key(&first, 3))), ["a", "b"]);
    }

    /// Fails if a slot is applied to only one of the two selector sets, or if
    /// a held slot silences a window it does not belong to.
    #[test]
    fn a_slot_narrows_both_selector_sets_of_its_own_window_only() {
        let mut rig = Rig::synthetic();
        let (held, other) = (window("held"), window("other"));
        let holder = key_subscriber(&mut rig, "holder", &WindowSelector::One(held.clone()));
        key_subscriber(&mut rig, "outsider", &All);

        rig.take(holder, &held, Actor);

        assert_eq!(receivers(&rig.inject(&held, &key(&held, 1))), ["holder"]);
        assert_eq!(receivers(&rig.inject(&other, &key(&other, 1))), ["outsider"]);
    }

    /// Fails if a slot outlives its window, if a window's close empties the
    /// slots of other windows, or if the holder is never told.
    #[test]
    fn a_closing_window_takes_its_slot_and_tells_the_holder() {
        let mut rig = Rig::synthetic();
        let spec = WindowSpec {
            name: "main".to_owned(),
            title: "Main".to_owned(),
            mode: WindowMode::Windowed,
            size: None,
            presentation: WindowPresentation::Display,
        };
        rig.send(&CreateWindow { spec });
        assert!(matches!(rig.reply::<CreateWindowResult>(), CreateWindowResult::Ok { .. }), "the window opens");
        let endpoint = rig
            .chassis()
            .child::<WindowCapability, WindowInstance>(rig.manager(), LoadName::new("main").expect("fixture name"))
            .expect("the window child is live");
        let (main, side) = (window("main"), window("side"));
        let holder = key_subscriber(&mut rig, "holder", &All);
        key_subscriber(&mut rig, "other", &All);
        rig.take(holder, &main, Actor);
        rig.take(holder, &side, Actor);

        let (_, closed) = rig.send_to(endpoint, &CloseWindow);

        assert!(matches!(rig.reply::<CloseWindowResult>(), CloseWindowResult::Ok), "the window closes");
        assert_eq!(heard(&closed), [("holder", lost(&main))]);
        assert_eq!(receivers(&rig.inject(&side, &key(&side, 1))), ["holder"], "the other window's slot stands");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 1))), ["holder", "other"]);
    }

    /// Fails if a window's slot is dropped with its key records when the
    /// window loses operating-system focus.
    #[test]
    fn a_slot_stands_while_its_window_is_unfocused() {
        let mut rig = Rig::synthetic();
        let holder = key_subscriber(&mut rig, "holder", &All);
        key_subscriber(&mut rig, "other", &All);
        let main = window("main");
        rig.take(holder, &main, Actor);

        rig.inject(&main, &WindowFocus { window: main.clone(), focused: false });
        rig.inject(&main, &WindowFocus { window: main.clone(), focused: true });

        assert_eq!(receivers(&rig.inject(&main, &key(&main, 1))), ["holder"]);
    }

    /// Fails if a subtree scope leaves out the holder's child, if the prefix
    /// test ignores the step boundary (`pan` is a textual prefix of `panel`
    /// and not its ancestor), if the scope of a take is ignored, or if a
    /// second take by the holder is treated as a change of holder.
    #[test]
    fn a_subtree_scope_admits_the_holders_descendants_and_no_one_else() {
        let mut rig = Rig::synthetic();
        let pan = key_subscriber(&mut rig, "pan", &All);
        key_subscriber(&mut rig, "panel", &All);
        rig.send_to(pan, &SpawnChild { key: "knob".to_owned() });
        let knob = WindowSubscription::Key(watcher_child("pan", "knob").narrow());
        assert!(matches!(rig.subscribe(All, knob), SubscribeWindowResult::Ok), "the child is live and subscribes");
        let main = window("main");

        rig.take(pan, &main, Subtree);
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 1))), ["pan", "pan/knob"]);

        let retaken = rig.take(pan, &main, Actor);
        assert!(retaken.is_empty(), "the holder taking again is told nothing");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 2))), ["pan"]);
    }

    /// Fails if a replaced holder is not told, if the slot is a stack that
    /// hands key focus back, or if a release by an actor that is not the
    /// holder empties the slot.
    #[test]
    fn the_latest_take_wins_and_nothing_is_handed_back() {
        let mut rig = Rig::synthetic();
        let first = key_subscriber(&mut rig, "first", &All);
        let second = key_subscriber(&mut rig, "second", &All);
        let main = window("main");
        rig.take(first, &main, Actor);

        let taken = rig.take(second, &main, Actor);
        assert_eq!(heard(&taken), [("first", lost(&main))]);
        assert_eq!(heard(&taken), [("second", gained(&main))]);

        assert!(rig.release(first, &main).is_empty(), "a release by the replaced holder changes nothing");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 1))), ["second"]);

        let released = rig.release(second, &main);
        assert_eq!(heard(&released), [("second", lost(&main))]);
        assert!(heard::<KeyFocusGained>(&released).is_empty(), "key focus is not handed back");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 2))), ["first", "second"]);
    }

    /// Fails if a slot outlives its holder: once the holder's departure is
    /// processed, every window it held sends keys to every subscriber again.
    #[test]
    fn a_departed_holder_empties_every_slot_it_held() {
        let mut rig = Rig::synthetic();
        let holder = key_subscriber(&mut rig, "holder", &All);
        let survivor = key_subscriber(&mut rig, "survivor", &All);
        let (first, second) = (window("first"), window("second"));
        rig.take(holder, &first, Actor);
        rig.take(holder, &second, Actor);

        rig.send_to(holder, &Leave);
        rig.pump_until("the departure notice", |state| {
            recipients::<Key>(state.subscribers(), &first) == BTreeSet::from([survivor.erase()])
        });

        assert_eq!(receivers(&rig.inject(&first, &key(&first, 1))), ["survivor"]);
        assert_eq!(receivers(&rig.inject(&second, &key(&second, 1))), ["survivor"]);
    }

    /// Fails if a key held when key focus changes is stuck for whoever was
    /// sent its press, if its repeats reach the new holder instead, or if its
    /// record outlives its release.
    #[test]
    fn a_key_down_before_a_take_keeps_its_repeats_and_its_release() {
        let mut rig = Rig::synthetic();
        key_subscriber(&mut rig, "camera", &All);
        let console = key_subscriber(&mut rig, "console", &All);
        let main = window("main");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 17))), ["camera", "console"]);

        rig.take(console, &main, Actor);

        assert_eq!(receivers(&rig.inject(&main, &key(&main, 17))), ["camera", "console"], "the repeat");
        assert_eq!(receivers(&rig.inject(&main, &key_release(&main, 17))), ["camera", "console"], "the release");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 30))), ["console"], "a new code");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 17))), ["console"], "the same code, pressed again");
    }

    /// Fails if the release of a key is sent to subscribers that were never
    /// sent its press, because the holder released key focus in between.
    #[test]
    fn a_key_pressed_under_a_hold_releases_to_the_former_holder_alone() {
        let mut rig = Rig::synthetic();
        key_subscriber(&mut rig, "camera", &All);
        let console = key_subscriber(&mut rig, "console", &All);
        let main = window("main");
        rig.take(console, &main, Actor);
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 28))), ["console"]);

        rig.release(console, &main);

        assert_eq!(receivers(&rig.inject(&main, &key_release(&main, 28))), ["console"]);
    }

    /// Fails if a key record is kept past a release that never arrives: a
    /// window that loses operating-system focus forgets its keys, so the same
    /// code pressed again is a new press under the slot as it is then.
    #[test]
    fn an_unfocused_window_forgets_its_key_records() {
        let mut rig = Rig::synthetic();
        key_subscriber(&mut rig, "camera", &All);
        let console = key_subscriber(&mut rig, "console", &All);
        let main = window("main");
        assert_eq!(receivers(&rig.inject(&main, &key(&main, 17))), ["camera", "console"]);

        rig.inject(&main, &WindowFocus { window: main.clone(), focused: false });
        rig.take(console, &main, Actor);

        assert_eq!(receivers(&rig.inject(&main, &key(&main, 17))), ["console"]);
    }
}
