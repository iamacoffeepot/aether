use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::{HarnessOp, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, SubstrateHarness};
use aether_kinds::{Key, MouseMove};
use aether_window::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SyntheticWindowCapability, UnsubscribeWindow, WindowId,
    WindowMode, WindowSelector, WindowSizeRequest, WindowSpec,
};

fn window_mailbox() -> &'static str {
    SyntheticWindowCapability::NAMESPACE
}

fn spec(title: &str, width: u32, height: u32) -> WindowSpec {
    WindowSpec {
        name: title.to_owned(),
        title: title.to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width, height }),
    }
}

#[test]
fn synthetic_runtime_models_window_lifecycle_and_controls_in_memory() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic window harness");
    let result = harness
        .execute(vec![
            ("initial", HarnessOp::send_and_await(window_mailbox(), &ListWindows)),
            (
                "create-first",
                HarnessOp::send_and_await(window_mailbox(), &CreateWindow { spec: spec("first", 320, 200) }),
            ),
            (
                "title-first",
                HarnessOp::send_and_await(
                    window_mailbox(),
                    &SetWindowTitle { window: WindowId(1), title: "renamed".to_owned() },
                ),
            ),
            (
                "resize-first",
                HarnessOp::send_and_await(
                    window_mailbox(),
                    &SetWindowMode {
                        window: WindowId(1),
                        mode: WindowMode::Windowed,
                        width: Some(640),
                        height: Some(360),
                    },
                ),
            ),
            ("focus-first", HarnessOp::send_and_await(window_mailbox(), &FocusWindow { window: WindowId(1) })),
            ("redraw-first", HarnessOp::send_and_await(window_mailbox(), &RequestWindowRedraw { window: WindowId(1) })),
            (
                "create-second",
                HarnessOp::send_and_await(window_mailbox(), &CreateWindow { spec: spec("second", 800, 600) }),
            ),
            ("listed", HarnessOp::send_and_await(window_mailbox(), &ListWindows)),
            ("close-first", HarnessOp::send_and_await(window_mailbox(), &CloseWindow { window: WindowId(1) })),
            ("remaining", HarnessOp::send_and_await(window_mailbox(), &ListWindows)),
        ])
        .expect("synthetic window operations settle");

    assert_eq!(
        result.reply::<ListWindowsResult>("initial").expect("initial list reply"),
        ListWindowsResult::Ok { windows: Vec::new() },
    );
    let CreateWindowResult::Ok { window: first } =
        result.reply::<CreateWindowResult>("create-first").expect("first create reply")
    else {
        panic!("first create succeeds");
    };
    assert_eq!(first.id, WindowId(1));
    assert_eq!(first.name, "first");
    assert_eq!((first.width, first.height), (320, 200));
    assert_eq!(
        result.reply::<SetWindowTitleResult>("title-first").expect("title reply"),
        SetWindowTitleResult::Ok { window: WindowId(1), title: "renamed".to_owned() },
    );
    assert_eq!(
        result.reply::<SetWindowModeResult>("resize-first").expect("mode reply"),
        SetWindowModeResult::Ok { window: WindowId(1), mode: WindowMode::Windowed, width: 640, height: 360 },
    );
    assert_eq!(
        result.reply::<FocusWindowResult>("focus-first").expect("focus reply"),
        FocusWindowResult::Ok { window: WindowId(1) },
    );
    assert_eq!(
        result.reply::<RequestWindowRedrawResult>("redraw-first").expect("redraw reply"),
        RequestWindowRedrawResult::Ok { window: WindowId(1) },
    );
    let CreateWindowResult::Ok { window: second } =
        result.reply::<CreateWindowResult>("create-second").expect("second create reply")
    else {
        panic!("second create succeeds");
    };
    assert_eq!(second.id, WindowId(2));
    assert_eq!(second.name, "second");

    let ListWindowsResult::Ok { windows } = result.reply::<ListWindowsResult>("listed").expect("populated list reply")
    else {
        panic!("list succeeds");
    };
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [WindowId(1), WindowId(2)]);
    assert_eq!(windows[0].title, "renamed");
    assert_eq!(windows[0].name, "first");
    assert_eq!((windows[0].width, windows[0].height), (640, 360));
    assert!(windows[0].focused);
    assert_eq!(
        result.reply::<CloseWindowResult>("close-first").expect("close reply"),
        CloseWindowResult::Ok { window: WindowId(1) },
    );
    let ListWindowsResult::Ok { windows } =
        result.reply::<ListWindowsResult>("remaining").expect("remaining list reply")
    else {
        panic!("list succeeds");
    };
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [WindowId(2)]);
}

#[test]
fn synthetic_events_route_by_selector_deduplicate_unsubscribe_and_settle() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic window harness");
    harness
        .execute(vec![
            (
                "create-first",
                HarnessOp::send_and_await(window_mailbox(), &CreateWindow { spec: spec("first", 320, 200) }),
            ),
            (
                "create-second",
                HarnessOp::send_and_await(window_mailbox(), &CreateWindow { spec: spec("second", 640, 360) }),
            ),
        ])
        .expect("create routed windows");

    let observer = MailboxId::from_name(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME);
    harness
        .execute(vec![
            (
                "key-all",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&SubscribeWindow {
                    selector: WindowSelector::All,
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            (
                "key-second",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&SubscribeWindow {
                    selector: WindowSelector::One(WindowId(2)),
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            ("key-first-event", HarnessOp::window_event(WindowId(1), &Key { window: WindowId(1), code: 11 })),
            ("key-second-event", HarnessOp::window_event(WindowId(2), &Key { window: WindowId(2), code: 22 })),
        ])
        .expect("overlapping key subscriptions settle through observer");
    assert_eq!(harness.count_observed(Key::NAME), 2, "All plus One must deduplicate the second window recipient");

    harness
        .execute(vec![
            (
                "move-second",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&SubscribeWindow {
                    selector: WindowSelector::One(WindowId(2)),
                    kind: MouseMove::ID,
                    mailbox: observer,
                }),
            ),
            (
                "move-first-event",
                HarnessOp::window_event(WindowId(1), &MouseMove { window: WindowId(1), x: 1.0, y: 2.0 }),
            ),
            (
                "move-second-event",
                HarnessOp::window_event(WindowId(2), &MouseMove { window: WindowId(2), x: 3.0, y: 4.0 }),
            ),
        ])
        .expect("specific selector events settle through observer");
    assert_eq!(harness.count_observed(MouseMove::NAME), 1, "One must reject the other window");

    harness
        .execute(vec![
            (
                "unsubscribe-all-selector",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&UnsubscribeWindow {
                    selector: WindowSelector::All,
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            (
                "key-first-after-unsubscribe",
                HarnessOp::window_event(WindowId(1), &Key { window: WindowId(1), code: 33 }),
            ),
            ("key-second-still-specific", HarnessOp::window_event(WindowId(2), &Key { window: WindowId(2), code: 44 })),
            (
                "unsubscribe-second-selector",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&UnsubscribeWindow {
                    selector: WindowSelector::One(WindowId(2)),
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            (
                "key-second-after-unsubscribe",
                HarnessOp::window_event(WindowId(2), &Key { window: WindowId(2), code: 55 }),
            ),
        ])
        .expect("unsubscribe operations and descendant observer mail settle");
    assert_eq!(
        harness.count_observed(Key::NAME),
        3,
        "only the still-specific second-window route should survive the first unsubscribe",
    );
}
