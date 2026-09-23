//! The synthetic window runtime driven through a [`SubstrateHarness`]: staged
//! creation, root-routed per-window commands, a window endpoint's own native
//! chrome commands, and an unexpected child departure.
//!
//! These scenarios address the runtime by the proven references the harness
//! hands out, which are typed by the `aether-window` the harness composes — so
//! they live here, against that crate, rather than in the runtime's unit tests,
//! whose test build is a separate copy of the crate with its own actor types.

use std::collections::BTreeMap;

use aether_actor::Addressable;
use aether_data::LoadName;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_window::{
    CreateWindow, CreateWindowResult, CursorIcon, ListWindows, ListWindowsResult, SetWindowCursor,
    SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult, SetWindowTitle, SetWindowTitleResult,
    SyntheticWindowCapability, SyntheticWindowInstance, WindowCapability, WindowId, WindowInstance, WindowMenu,
    WindowMode, WindowSpec,
};

/// Local twin of the runtime's crate-private `RetireWindow`
/// (`aether.window.internal.retire`) — the manager-private request a window
/// child retires itself on. Same `#[kind(name)]` and shape, so the `KindId`
/// and wire bytes match; the child is addressed erased because the twin is not
/// the kind its handler names.
#[aether_data::kind(name = "aether.window.internal.retire", copy, eq)]
struct RetireWindow;

fn spec(name: &str, title: &str) -> WindowSpec {
    WindowSpec { name: name.to_owned(), title: title.to_owned(), mode: WindowMode::Windowed, size: None }
}

fn window_key(name: &str) -> LoadName {
    LoadName::new(name).expect("a valid window name")
}

/// Scheduler-backed. `CreateWindow` now stages its child and answers from a
/// later turn, so this pins the ordering the staged path has to preserve:
/// by the time the caller sees `Ok`, the window is already enumerable and
/// its child already owns the name a second create is refused for. A
/// promotion that replied before publishing would list an empty set here.
#[test]
fn create_replies_only_after_the_staged_child_is_live() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
    let report = harness
        .execute(vec![
            (
                "created",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &CreateWindow { spec: spec("main", "Main") },
                ),
            ),
            (
                "listed",
                HarnessOp::send_and_await_reply(&harness.actor_ref::<SyntheticWindowCapability>(), &ListWindows),
            ),
            (
                "duplicate",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &CreateWindow { spec: spec("main", "Second") },
                ),
            ),
        ])
        .expect("staged window creation settles");

    let Ok(CreateWindowResult::Ok { window }) = report.reply::<CreateWindowResult>("created") else {
        panic!("staged create succeeds");
    };
    assert_eq!(window.id, WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "main").0));

    let Ok(ListWindowsResult::Ok { windows }) = report.reply::<ListWindowsResult>("listed") else {
        panic!("synthetic list succeeds");
    };
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [window.id]);

    assert!(matches!(report.reply::<CreateWindowResult>("duplicate"), Ok(CreateWindowResult::Err { .. })));
}

/// The seven per-window commands are the *endpoint's* handlers, so a
/// root-addressed one used to fall through the manager's dispatch and
/// settle with no reply and no effect — which reads as success to a caller
/// that sees only the status (iamacoffeepot/aether#5505).
///
/// The sole-window case has to come back as the endpoint's own
/// `Ok { title }` under the *caller's* correlation and leave the window
/// retitled, which is the whole of the re-dispatch: an answer minted by the
/// root, or none at all, fails here. The zero- and two-window cases are
/// what stops the root from quietly picking a window instead.
#[test]
fn root_addressed_commands_reach_the_sole_window_and_refuse_when_it_is_ambiguous() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
    let report = harness
        .execute(vec![
            (
                "windowless",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &SetWindowTitle { title: "Nobody".to_owned() },
                ),
            ),
            (
                "created",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &CreateWindow { spec: spec("main", "Main") },
                ),
            ),
            (
                "routed",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &SetWindowTitle { title: "Routed".to_owned() },
                ),
            ),
            (
                "second",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &CreateWindow { spec: spec("palette", "Tools") },
                ),
            ),
            (
                "ambiguous",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<SyntheticWindowCapability>(),
                    &SetWindowTitle { title: "Ambiguous".to_owned() },
                ),
            ),
            (
                "listed",
                HarnessOp::send_and_await_reply(&harness.actor_ref::<SyntheticWindowCapability>(), &ListWindows),
            ),
        ])
        .expect("root-addressed window commands settle");

    assert!(
        matches!(report.reply::<SetWindowTitleResult>("windowless"), Ok(SetWindowTitleResult::Err { .. })),
        "a root-addressed command with no window to route to is refused, not swallowed",
    );
    assert!(matches!(
        report.reply::<SetWindowTitleResult>("routed"),
        Ok(SetWindowTitleResult::Ok { title }) if title == "Routed"
    ));
    assert!(
        matches!(report.reply::<SetWindowTitleResult>("ambiguous"), Ok(SetWindowTitleResult::Err { .. })),
        "with several windows live the root names the requirement rather than choosing one",
    );

    let Ok(ListWindowsResult::Ok { windows }) = report.reply::<ListWindowsResult>("listed") else {
        panic!("synthetic list succeeds");
    };
    assert_eq!(
        windows.iter().map(|window| (window.name.as_str(), window.title.as_str())).collect::<BTreeMap<_, _>>(),
        BTreeMap::from([("main", "Routed"), ("palette", "Tools")]),
        "the routed command applied to the sole window and the refused one applied to nothing",
    );
}

/// A new per-window command has four places to be wired — the endpoint's
/// forwarding handler, the manager's apply arm, the endpoint's
/// correlation arm in `complete`, and its shutdown arm in `unwire` — and
/// only the first is compile-checked. A missing apply arm leaves the
/// caller with no reply; a missing correlation arm `fatal_abort`s the
/// endpoint on the way back. Driving both new commands through a live
/// window's own mailbox and reading their replies is what covers the round
/// trip the reducer-only tests cannot see.
#[test]
fn the_native_chrome_commands_round_trip_through_a_windows_own_endpoint() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
    let window = harness.actor_ref::<SyntheticWindowCapability>();
    harness
        .execute(vec![(
            "created",
            HarnessOp::send_and_await_reply(&window, &CreateWindow { spec: spec("main", "Main") }),
        )])
        .expect("the window opens");
    let main = harness
        .child::<SyntheticWindowCapability, SyntheticWindowInstance>(&window, window_key("main"))
        .expect("the main window is live");
    let report = harness
        .execute(vec![
            (
                "menu",
                HarnessOp::send_and_await_reply(
                    &main,
                    &SetWindowMenu { menus: vec![WindowMenu { title: "File".to_owned(), items: Vec::new() }] },
                ),
            ),
            ("cursor", HarnessOp::send_and_await_reply(&main, &SetWindowCursor { icon: CursorIcon::ResizeHorizontal })),
        ])
        .expect("native chrome commands settle at the window endpoint");

    assert!(matches!(report.reply::<SetWindowMenuResult>("menu"), Ok(SetWindowMenuResult::Ok)));
    assert!(matches!(report.reply::<SetWindowCursorResult>("cursor"), Ok(SetWindowCursorResult::Ok)));
}

#[test]
fn unexpected_child_departure_closes_only_its_window() {
    let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
    let window = harness.actor_ref::<SyntheticWindowCapability>();
    harness
        .execute(vec![
            ("first", HarnessOp::send_and_await_reply(&window, &CreateWindow { spec: spec("first", "First") })),
            ("second", HarnessOp::send_and_await_reply(&window, &CreateWindow { spec: spec("second", "Second") })),
        ])
        .expect("both windows open");
    let first = harness
        .child::<SyntheticWindowCapability, SyntheticWindowInstance>(&window, window_key("first"))
        .expect("the first window is live");
    harness
        .execute(vec![
            ("depart-first", HarnessOp::send_and_settle(first.erase(), &RetireWindow)),
            ("remaining", HarnessOp::send_and_await_reply(&window, &ListWindows)),
        ])
        .expect("unexpected child departure settles");

    let second = WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "second").0);

    // The departure's own chain settles with `RetireWindow`, but the
    // `MonitorNotice` that prunes the capability's list (ADR-0079 §8)
    // reaches the parent on a chain the caller never joins — so there is
    // nothing here to await, only a state to observe becoming true
    // (iamacoffeepot/aether#4184). That is what `poll_until` is for: a
    // wall-clock budget rather than a round-trip count, so the test
    // measures the outcome rather than the runner, and a timeout names
    // the list it actually last saw.
    let listed = harness
        .execute(vec![(
            "listed",
            HarnessOp::poll_until(
                &harness.actor_ref::<SyntheticWindowCapability>(),
                &ListWindows,
                move |reply: &ListWindowsResult| {
                    matches!(reply, ListWindowsResult::Ok { windows }
                    if windows.iter().map(|window| window.id).eq([second]))
                },
            ),
        )])
        .expect("the retired child's window is pruned from the capability's list");

    let Ok(ListWindowsResult::Ok { windows }) = listed.reply::<ListWindowsResult>("listed") else {
        panic!("synthetic list succeeds");
    };
    assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [second]);
}
