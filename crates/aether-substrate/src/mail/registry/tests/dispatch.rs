//! Tests for [`super::super::dispatch`] — the settlement obligation an
//! `OwnedDispatch` carries and the per-mail metadata it forwards.

use std::panic;
use std::sync::{Arc, mpsc};

use aether_kinds::trace::Nanos;

use crate::mail::mailer::Mailer;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
use crate::mail::{KindId, Mail, MailId, MailRef, MailboxId, Source, SourceAddr};
use crate::testing::boot_authority as auth;

/// ADR-0094: a fresh armed [`OwnedDispatch`] panics on drop if it was
/// neither discharged nor transferred — the headline regression gate
/// for the #846 / #1325 dropped-bracket class. Debug-only (the guard
/// is compiled out in release).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "settlement-obligation leak")]
fn armed_dispatch_panics_if_dropped_without_discharge() {
    let env = OwnedDispatch::armed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(vec![1u8, 2, 3]),
        1,
        MailId::new(MailboxId(42), 9),
        MailId::new(MailboxId(42), 9),
        None,
        Nanos(0),
        0,
        MailboxId(42),
    );
    // Drop without discharge/transfer — the InboxHandler contract
    // violation. The panic message names the offending seam.
    drop(env);
}

/// ADR-0094: the panic message names `mail_id` + the kind so the leaking seam
/// is locatable, not anonymous.
///
/// Asserted through `catch_unwind` rather than `#[should_panic(expected = ...)]`
/// because the identifying token is now the kind *id* — a value with no literal
/// spelling to paste into an attribute (iamacoffeepot/aether#4278). Rendering it
/// here and asserting containment keeps the claim the test was making instead of
/// weakening it to "something panicked".
#[cfg(debug_assertions)]
#[test]
fn armed_dispatch_panic_names_the_kind() {
    let leaked = KindId(7);
    let payload = panic::catch_unwind(|| {
        let env = OwnedDispatch::armed(
            leaked,
            None,
            Source::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(MailboxId(1), 1),
            MailId::new(MailboxId(1), 1),
            None,
            Nanos(0),
            0,
            MailboxId(1),
        );
        drop(env);
    })
    .expect_err("an armed dispatch dropped undischarged must panic");

    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("the obligation guard panics with a message");
    assert!(message.contains(&format!("{leaked:?}")), "the leak panic must name the leaking kind; got: {message}");
}

/// ADR-0094: an armed dispatch that is `discharge()`d before drop
/// does NOT panic — the consumer recorded `Finished`.
#[test]
fn discharged_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(2), 2),
        MailId::new(MailboxId(2), 2),
        None,
        Nanos(0),
        0,
        MailboxId(2),
    );
    env.discharge();
    drop(env);
}

/// ADR-0114 decision #1: the routed recipient promoted to a real
/// `OwnedDispatch` field survives in every build (not just the
/// debug-only `ObligationGuard`). Both mint sites stamp it from
/// their `recipient` parameter; a clone (for inspection) carries it
/// through too.
#[test]
fn dispatch_carries_routed_recipient() {
    let recipient = MailboxId(0xABCD);
    let env = OwnedDispatch::disarmed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(3), 3),
        MailId::new(MailboxId(3), 3),
        None,
        Nanos(0),
        0,
        recipient,
    );
    assert_eq!(env.recipient, recipient);
    // The hand-rolled `Clone` must propagate the new field — a clone
    // is for inspection, but still carries the recipient.
    let cloned = env.clone();
    drop(env);
    assert_eq!(cloned.recipient, recipient);
}

/// ADR-0094: an armed dispatch that is `mark_transferred()` before
/// drop does NOT panic — the obligation moved onward.
#[test]
fn transferred_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(3), 3),
        MailId::new(MailboxId(3), 3),
        None,
        Nanos(0),
        0,
        MailboxId(3),
    );
    env.mark_transferred();
    drop(env);
}

/// ADR-0094: a disarmed mint (the test/helper path) never panics on
/// drop even without discharge.
#[test]
fn disarmed_dispatch_does_not_panic() {
    let env = OwnedDispatch::disarmed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    );
    drop(env);
}

/// ADR-0094 `Clone` note: cloning an armed dispatch produces a
/// **disarmed** clone (a clone is for inspection, never a second
/// obligation), so dropping the clone does not panic. The original is
/// discharged to keep the test itself clean.
#[cfg(debug_assertions)]
#[test]
fn clone_of_armed_dispatch_is_disarmed() {
    let env = OwnedDispatch::armed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(vec![9u8]),
        1,
        MailId::new(MailboxId(4), 4),
        MailId::new(MailboxId(4), 4),
        None,
        Nanos(0),
        0,
        MailboxId(4),
    );
    let clone = env.clone();
    // The clone carries no obligation — dropping it must not panic.
    drop(clone);
    // Original still armed: discharge so the test exits cleanly.
    env.discharge();
}

/// ADR-0094 issue 1326: arming a `MailId::NONE` dispatch mints **no**
/// obligation — `record_finished` no-ops on `MailId::NONE`, so the
/// chassis-internal fire-and-forget pushes that stamp it (RPC
/// self-pokes like `aether.rpc.inbound_ready`, window pushes) route
/// through the armed `Inbox` arm but never discharge. The arm site is
/// unconditional; `ObligationGuard::armed` disarms on NONE so the
/// guard's arm condition matches `record_finished` exactly. Dropping
/// such a dispatch without discharge must NOT panic.
#[cfg(debug_assertions)]
#[test]
fn armed_none_mail_id_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(63),
    );
    // No discharge / transfer — a NONE dispatch carries no obligation,
    // so the guard must be disarmed and the drop must be silent.
    drop(env);
}

/// ADR-0094 no-leak side of the headline coverage: routing a real mail
/// through the standard actor dispatcher (`DispatcherSlot::dispatch_one`
/// via `register_inbox` + a seized run) discharges the obligation, so
/// no guard panic fires on the production drain path.
#[test]
fn standard_inbox_handler_relay_does_not_panic() {
    // The `register_inbox` relay closure moves the armed dispatch onto
    // a channel (a transfer); the channel's receiver here drains and
    // discharges it explicitly, mirroring `dispatch_one`. A panic here
    // would mean the relay/transfer path false-positives.
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<OwnedDispatch>();
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        // Relay: the value moves onto the channel, carrying its
        // obligation. No discharge here — the drainer below owns it.
        let _ = tx.send(dispatch);
    });
    // Mint armed exactly as `route_mail`'s Inbox arm does.
    handler.enqueue(OwnedDispatch::armed(
        KindId(11),
        None,
        Source::NONE,
        MailRef::from(vec![0u8]),
        1,
        MailId::new(MailboxId(5), 5),
        MailId::new(MailboxId(5), 5),
        None,
        Nanos(0),
        0,
        MailboxId(5),
    ));
    let env = rx.recv().expect("relay forwarded the dispatch");
    // Downstream dispatcher discharges (the `dispatch_one` template).
    env.discharge();
    drop(env);
}

#[test]
fn repeated_routed_inbox_mail_preserves_per_mail_metadata() {
    let registry = Arc::new(Registry::new());
    let kind = registry.register_kind(&auth(), "aether.shared.kind");
    let (tx, rx) = mpsc::channel();
    let recipient = registry.register_inbox(
        &auth(),
        "shared-kind-recipient",
        Arc::new(move |dispatch: OwnedDispatch| {
            let _ = tx.send(dispatch);
        }),
    );
    let mailer = Mailer::new(Arc::clone(&registry));
    let first_id = MailId::new(MailboxId(11), 1);
    let second_id = MailId::new(MailboxId(11), 2);
    mailer.push(
        Mail::new(recipient, kind, vec![1, 2], 1)
            .with_reply_to(Source::with_correlation(SourceAddr::Component(MailboxId(11)), 41))
            .with_lineage(first_id, first_id, None),
    );
    mailer.push(
        Mail::new(recipient, kind, vec![3, 4], 2)
            .with_reply_to(Source::with_correlation(SourceAddr::Component(MailboxId(11)), 42))
            .with_lineage(second_id, first_id, Some(first_id)),
    );

    let first = rx.recv().expect("first routed dispatch");
    let second = rx.recv().expect("second routed dispatch");
    // Both carry the id they were pushed with — the routing step copies it
    // through rather than re-deriving it from anything.
    assert_eq!(first.kind, kind);
    assert_eq!(second.kind, kind);
    assert_eq!(first.payload.bytes(), [1, 2]);
    assert_eq!(second.payload.bytes(), [3, 4]);
    assert_eq!(second.count, 2);
    assert_eq!(second.mail_id, second_id);
    assert_eq!(second.root, first_id);
    assert_eq!(second.parent_mail, Some(first_id));
    first.discharge();
    second.discharge();
}
