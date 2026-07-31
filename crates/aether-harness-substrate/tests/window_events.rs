use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::{ExecutionResult, HarnessOp, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, SubstrateHarness};
use aether_kinds::{Key, MouseMove};
use aether_window::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SyntheticWindowCapability, UnsubscribeWindow,
    WindowCapability, WindowId, WindowInstance, WindowMode, WindowSelector, WindowSizeRequest, WindowSpec,
};

fn window_mailbox() -> &'static str {
    WindowCapability::NAMESPACE
}

fn window_id(name: &str) -> WindowId {
    WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, name).0)
}

fn canonical_window(name: &str) -> String {
    format!("{}/{}:{name}", WindowCapability::NAMESPACE, WindowInstance::NAMESPACE)
}

fn abbreviated_window(name: &str) -> String {
    format!("{}://{name}", WindowCapability::NAMESPACE)
}

fn spec(title: &str, width: u32, height: u32) -> WindowSpec {
    WindowSpec {
        name: title.to_owned(),
        title: title.to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width, height }),
    }
}

fn assert_window_lifecycle(result: &ExecutionResult, first_id: WindowId, second_id: WindowId) {
    assert_eq!(
        result.reply::<ListWindowsResult>("initial").expect("initial list reply"),
        ListWindowsResult::Ok { windows: Vec::new() },
    );
    let CreateWindowResult::Ok { window: first } =
        result.reply::<CreateWindowResult>("create-first").expect("first create reply")
    else {
        panic!("first create succeeds");
    };
    assert_eq!(first.id, first_id);
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
        result.reply::<CreateWindowResult>("create-second").expect("second create reply")
    else {
        panic!("second create succeeds");
    };
    assert_eq!(second.id, second_id);
    assert_eq!(second.name, "second");

    let ListWindowsResult::Ok { windows } = result.reply::<ListWindowsResult>("listed").expect("populated list reply")
    else {
        panic!("list succeeds");
    };
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), {
        let mut ids = vec![first_id, second_id];
        ids.sort_unstable();
        ids
    });
    let first = windows.iter().find(|window| window.id == first_id).expect("first listed");
    assert_eq!(first.title, "renamed");
    assert_eq!(first.name, "first");
    assert!(first.focused);
    let second = windows.iter().find(|window| window.id == second_id).expect("second listed");
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
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [second_id]);
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
        window_mailbox(),
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
    let first_id = window_id("first");
    let second_id = window_id("second");
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
                HarnessOp::send_and_await(canonical_window("first"), &SetWindowTitle { title: "renamed".to_owned() }),
            ),
            (
                "create-second",
                HarnessOp::send_and_await(window_mailbox(), &CreateWindow { spec: spec("second", 800, 600) }),
            ),
            (
                "resize-second",
                HarnessOp::send_and_await(
                    abbreviated_window("second"),
                    &SetWindowMode { mode: WindowMode::Windowed, width: Some(640), height: Some(360) },
                ),
            ),
            ("focus-first", HarnessOp::send_and_await(canonical_window("first"), &FocusWindow)),
            ("redraw-second", HarnessOp::send_and_await(abbreviated_window("second"), &RequestWindowRedraw)),
            ("listed", HarnessOp::send_and_await(window_mailbox(), &ListWindows)),
            ("close-first", HarnessOp::send_and_await(canonical_window("first"), &CloseWindow)),
            (
                "title-second-after-close",
                HarnessOp::send_and_await(
                    abbreviated_window("second"),
                    &SetWindowTitle { title: "survivor".to_owned() },
                ),
            ),
            ("remaining", HarnessOp::send_and_await(window_mailbox(), &ListWindows)),
        ])
        .expect("synthetic window operations settle");

    assert_window_lifecycle(&result, first_id, second_id);
    assert_closed_subname_retires(&mut harness);
}

#[test]
fn synthetic_events_route_by_selector_deduplicate_unsubscribe_and_settle() {
    let first_id = window_id("first");
    let second_id = window_id("second");
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
                    selector: WindowSelector::One(second_id),
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            ("key-first-event", HarnessOp::window_event(first_id, &Key { window: first_id, code: 11 })),
            ("key-second-event", HarnessOp::window_event(second_id, &Key { window: second_id, code: 22 })),
        ])
        .expect("overlapping key subscriptions settle through observer");
    assert_eq!(harness.count_observed(Key::NAME), 2, "All plus One must deduplicate the second window recipient");

    harness
        .execute(vec![
            (
                "move-second",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&SubscribeWindow {
                    selector: WindowSelector::One(second_id),
                    kind: MouseMove::ID,
                    mailbox: observer,
                }),
            ),
            ("move-first-event", HarnessOp::window_event(first_id, &MouseMove { window: first_id, x: 1.0, y: 2.0 })),
            ("move-second-event", HarnessOp::window_event(second_id, &MouseMove { window: second_id, x: 3.0, y: 4.0 })),
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
            ("key-first-after-unsubscribe", HarnessOp::window_event(first_id, &Key { window: first_id, code: 33 })),
            ("key-second-still-specific", HarnessOp::window_event(second_id, &Key { window: second_id, code: 44 })),
            (
                "unsubscribe-second-selector",
                HarnessOp::actor::<SyntheticWindowCapability>().send(&UnsubscribeWindow {
                    selector: WindowSelector::One(second_id),
                    kind: Key::ID,
                    mailbox: observer,
                }),
            ),
            ("key-second-after-unsubscribe", HarnessOp::window_event(second_id, &Key { window: second_id, code: 55 })),
        ])
        .expect("unsubscribe operations and descendant observer mail settle");
    assert_eq!(
        harness.count_observed(Key::NAME),
        3,
        "only the still-specific second-window route should survive the first unsubscribe",
    );
}
