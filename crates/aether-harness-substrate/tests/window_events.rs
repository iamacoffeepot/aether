use aether_actor::{ActorPath, ActorRef, actor};
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::{ExecutionResult, HarnessOp, SubstrateHarness};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx};
use aether_test_fixtures_kinds::SubstrateHarnessObserver;
use aether_window::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, UnsubscribeWindow, WindowCapability, WindowInstance,
    WindowMode, WindowPresentation, WindowSelector, WindowSizeRequest, WindowSpec, WindowSubscription, window_path,
};
use aether_window::{Key, MouseMove};

/// The scenario's subscriber: silent `Key` and `MouseMove` handlers, so its
/// path narrows to a subscriber of each, that forward every event to the
/// harness observer, where `count_observed` counts it. Each handler owns the
/// event it was dispatched and drops it once forwarded.
struct Relay;

#[actor(singleton, root, depends(SubstrateHarnessObserver))]
impl NativeActor for Relay {
    const NAMESPACE: &'static str = "test.window_events.relay";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::event]
    fn on_key(&mut self, ctx: &mut NativeCtx<'_>, key: Key) {
        let _ = self;
        ctx.send::<SubstrateHarnessObserver>(&key);
        drop(key);
    }

    #[handler::event]
    fn on_mouse_move(&mut self, ctx: &mut NativeCtx<'_>, mouse: MouseMove) {
        let _ = self;
        ctx.send::<SubstrateHarnessObserver>(&mouse);
        drop(mouse);
    }
}

/// The relay as a subscriber to `Key`.
fn keys() -> WindowSubscription {
    WindowSubscription::Key(ActorPath::<Relay>::root().narrow())
}

/// The relay as a subscriber to `MouseMove`.
fn moves() -> WindowSubscription {
    WindowSubscription::MouseMove(ActorPath::<Relay>::root().narrow())
}

fn window(name: &str) -> ErasedActorPath {
    window_path(&LoadName::new(name).expect("window name"))
}

/// Inject a `Key` press as coming from the window at `window`.
fn key_from(synthetic: ActorRef<WindowCapability>, window: &ErasedActorPath, code: u32) -> HarnessOp {
    HarnessOp::window_event(&synthetic, window.clone(), &Key { window: window.clone(), code })
}

/// Inject a `MouseMove` as coming from the window at `window`.
fn move_from(synthetic: ActorRef<WindowCapability>, window: &ErasedActorPath, x: f32, y: f32) -> HarnessOp {
    HarnessOp::window_event(&synthetic, window.clone(), &MouseMove { window: window.clone(), x, y })
}

/// The named window instance the synthetic window capability opened.
fn window_instance(harness: &SubstrateHarness, name: &str) -> ActorRef<WindowInstance> {
    let window = harness.actor_ref::<WindowCapability>();
    harness
        .child::<WindowCapability, WindowInstance>(&window, LoadName::new(name).expect("window name"))
        .unwrap_or_else(|error| panic!("window {name} is live: {error}"))
}

fn spec(title: &str, width: u32, height: u32) -> WindowSpec {
    WindowSpec {
        name: title.to_owned(),
        title: title.to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width, height }),
        presentation: WindowPresentation::Display,
    }
}

fn assert_window_lifecycle(
    created: &ExecutionResult,
    result: &ExecutionResult,
    first_path: &ErasedActorPath,
    second_path: &ErasedActorPath,
) {
    assert_eq!(
        created.reply::<ListWindowsResult>("initial").expect("initial list reply"),
        ListWindowsResult::Ok { windows: Vec::new() },
    );
    let CreateWindowResult::Ok { window: first } =
        created.reply::<CreateWindowResult>("create-first").expect("first create reply")
    else {
        panic!("first create succeeds");
    };
    assert_eq!(first.path, *first_path);
    assert_eq!(first.name, "first");
    assert_eq!((first.width, first.height), (320, 200));
    assert_eq!(
        result.reply::<SetWindowTitleResult>("title-first").expect("title reply"),
        SetWindowTitleResult::Ok { title: "renamed".to_owned() },
    );
    assert_eq!(
        result.reply::<SetWindowModeResult>("resize-second").expect("mode reply"),
        SetWindowModeResult::Ok { mode: WindowMode::Windowed, width: 640, height: 360 },
    );
    assert_eq!(result.reply::<FocusWindowResult>("focus-first").expect("focus reply"), FocusWindowResult::Ok,);
    assert_eq!(
        result.reply::<RequestWindowRedrawResult>("redraw-second").expect("redraw reply"),
        RequestWindowRedrawResult::Ok,
    );
    let CreateWindowResult::Ok { window: second } =
        created.reply::<CreateWindowResult>("create-second").expect("second create reply")
    else {
        panic!("second create succeeds");
    };
    assert_eq!(second.path, *second_path);
    assert_eq!(second.name, "second");

    let ListWindowsResult::Ok { windows } = result.reply::<ListWindowsResult>("listed").expect("populated list reply")
    else {
        panic!("list succeeds");
    };
    assert_eq!(windows.iter().map(|window| &window.path).collect::<Vec<_>>(), [first_path, second_path]);
    let first = windows.iter().find(|window| window.path == *first_path).expect("first listed");
    assert_eq!(first.title, "renamed");
    assert_eq!(first.name, "first");
    assert!(first.focused);
    let second = windows.iter().find(|window| window.path == *second_path).expect("second listed");
    assert_eq!((second.width, second.height), (640, 360));
    assert_eq!(result.reply::<CloseWindowResult>("close-first").expect("close reply"), CloseWindowResult::Ok,);
    assert_eq!(
        result.reply::<SetWindowTitleResult>("title-second-after-close").expect("surviving sibling title reply"),
        SetWindowTitleResult::Ok { title: "survivor".to_owned() },
    );
    let ListWindowsResult::Ok { windows } =
        result.reply::<ListWindowsResult>("remaining").expect("remaining list reply")
    else {
        panic!("list succeeds");
    };
    assert_eq!(windows.iter().map(|window| &window.path).collect::<Vec<_>>(), [second_path]);
    assert_eq!(windows[0].title, "survivor");
}

/// A closed window's subname must end up retired: re-creating it answers
/// `SubnameRetired`, the authoritative reason, rather than handing the name back.
///
/// `CloseWindowResult::Ok` leaves the instance's handler before the teardown is
/// even requested, and the retirement lands much later — on the slot's own
/// teardown turn, on a chain no caller joins — where
/// `finalize_close_and_fan_out` tombstones the id and then releases the parent's
/// live-child key. Both pieces gate the answer: while either is outstanding the
/// parent still holds the subname locally and refuses with the stale
/// `SubnameInUse`. Production promises the authoritative reason only to a
/// watcher acting on the `MonitorNotice` that fan-out sends, and a test cannot
/// register one, so this polls to a wall-clock budget rather than counting
/// round-trips — the same shape iamacoffeepot/aether#4184 needed for the sibling
/// prune, and what `HarnessOp::poll_until` exists to express.
///
/// Retrying the create is safe because it cannot succeed: retiring an actor
/// leaves its route in place and tombstones the id instead, so every re-birth at
/// a lived-in id hits the registry owner's route-conflict arm and the race
/// decides only which refusal comes back. An `Ok` would mean that invariant
/// broke, so the observation fails on it instead of retrying it away — the poll
/// can never go green having leaked a live window.
fn assert_closed_subname_retires(harness: &mut SubstrateHarness) {
    let retired = HarnessOp::poll_until(
        &harness.actor_ref::<WindowCapability>(),
        &CreateWindow { spec: spec("first", 320, 200) },
        |reply: &CreateWindowResult| match reply {
            CreateWindowResult::Ok { .. } => {
                panic!("re-creating a closed window's subname must be refused, got {reply:?}")
            }
            CreateWindowResult::Err { error } if error.contains("SubnameRetired") => true,
            CreateWindowResult::Err { error } => {
                assert!(error.contains("SubnameInUse"), "unexpected refusal for a closed window's subname: {error}");
                false
            }
        },
    );

    harness
        .execute(vec![("retired-name", retired)])
        .expect("the closed window's subname answers the authoritative SubnameRetired");
}

#[test]
fn synthetic_runtime_models_window_lifecycle_and_controls_in_memory() {
    let first_path = window("first");
    let second_path = window("second");
    let mut harness = SubstrateHarness::start().expect("boot synthetic window harness");
    let window = harness.actor_ref::<WindowCapability>();
    let created = harness
        .execute(vec![
            ("initial", HarnessOp::send_and_await_reply(&window, &ListWindows)),
            ("create-first", HarnessOp::send_and_await_reply(&window, &CreateWindow { spec: spec("first", 320, 200) })),
            (
                "create-second",
                HarnessOp::send_and_await_reply(&window, &CreateWindow { spec: spec("second", 800, 600) }),
            ),
        ])
        .expect("synthetic windows open");

    let first = window_instance(&harness, "first");
    let second = window_instance(&harness, "second");
    let result = harness
        .execute(vec![
            ("title-first", HarnessOp::send_and_await_reply(&first, &SetWindowTitle { title: "renamed".to_owned() })),
            (
                "resize-second",
                HarnessOp::send_and_await_reply(
                    &second,
                    &SetWindowMode { mode: WindowMode::Windowed, width: Some(640), height: Some(360) },
                ),
            ),
            ("focus-first", HarnessOp::send_and_await_reply(&first, &FocusWindow)),
            ("redraw-second", HarnessOp::send_and_await_reply(&second, &RequestWindowRedraw)),
            ("listed", HarnessOp::send_and_await_reply(&window, &ListWindows)),
            ("close-first", HarnessOp::send_and_await_reply(&first, &CloseWindow)),
            (
                "title-second-after-close",
                HarnessOp::send_and_await_reply(&second, &SetWindowTitle { title: "survivor".to_owned() }),
            ),
            ("remaining", HarnessOp::send_and_await_reply(&window, &ListWindows)),
        ])
        .expect("synthetic window operations settle");

    assert_window_lifecycle(&created, &result, &first_path, &second_path);
    assert_closed_subname_retires(&mut harness);
}

#[test]
fn synthetic_events_route_by_selector_deduplicate_unsubscribe_and_settle() {
    let first_path = window("first");
    let second_path = window("second");
    let mut harness =
        SubstrateHarness::builder().with_actor::<Relay>(()).build().expect("boot synthetic window harness");
    let synthetic = harness.actor_ref::<WindowCapability>();
    harness
        .execute(vec![
            (
                "create-first",
                HarnessOp::send_and_await_reply(&synthetic, &CreateWindow { spec: spec("first", 320, 200) }),
            ),
            (
                "create-second",
                HarnessOp::send_and_await_reply(&synthetic, &CreateWindow { spec: spec("second", 640, 360) }),
            ),
        ])
        .expect("create routed windows");

    harness
        .execute(vec![
            (
                "key-all",
                HarnessOp::send_and_settle(
                    &synthetic,
                    &SubscribeWindow { selector: WindowSelector::All, subscription: keys() },
                ),
            ),
            (
                "key-second",
                HarnessOp::send_and_settle(
                    &synthetic,
                    &SubscribeWindow { selector: WindowSelector::One(second_path.clone()), subscription: keys() },
                ),
            ),
            ("key-first-event", key_from(synthetic, &first_path, 11)),
            ("key-second-event", key_from(synthetic, &second_path, 22)),
        ])
        .expect("overlapping key subscriptions settle through observer");
    assert_eq!(harness.count_observed(Key::NAME), 2, "All plus One must deduplicate the second window recipient");

    harness
        .execute(vec![
            (
                "move-second",
                HarnessOp::send_and_settle(
                    &synthetic,
                    &SubscribeWindow { selector: WindowSelector::One(second_path.clone()), subscription: moves() },
                ),
            ),
            ("move-first-event", move_from(synthetic, &first_path, 1.0, 2.0)),
            ("move-second-event", move_from(synthetic, &second_path, 3.0, 4.0)),
        ])
        .expect("specific selector events settle through observer");
    assert_eq!(harness.count_observed(MouseMove::NAME), 1, "One must reject the other window");

    harness
        .execute(vec![
            (
                "unsubscribe-all-selector",
                HarnessOp::send_and_settle(
                    &synthetic,
                    &UnsubscribeWindow { selector: WindowSelector::All, subscription: keys() },
                ),
            ),
            ("key-first-after-unsubscribe", key_from(synthetic, &first_path, 33)),
            ("key-second-still-specific", key_from(synthetic, &second_path, 44)),
            (
                "unsubscribe-second-selector",
                HarnessOp::send_and_settle(
                    &synthetic,
                    &UnsubscribeWindow { selector: WindowSelector::One(second_path.clone()), subscription: keys() },
                ),
            ),
            ("key-second-after-unsubscribe", key_from(synthetic, &second_path, 55)),
        ])
        .expect("unsubscribe operations and descendant observer mail settle");
    assert_eq!(
        harness.count_observed(Key::NAME),
        3,
        "only the still-specific second-window route should survive the first unsubscribe",
    );
}
