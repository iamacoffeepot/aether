//! Attached mail through the mailer's route: the entries ride the inbox
//! hand-off, and every plain-bytes exit sees tag-0 bytes or nothing.

use std::mem;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use aether_codec::decode_schema;
use aether_codec::frame::max_frame_size;
use aether_data::{Blob, Kind, KindDescriptor, Schema};
use serde_json::{Value, json};

use super::{Attachments, SharingEncoder};
use crate::mail::registry::{MailDispatch, OwnedDispatch};
use crate::mail::{EgressEvent, Mail, MailboxId, Mailer, Registry};
use crate::testing::{bare_substrate, boot_authority, test_mailer_and_rx};

/// A mailbox id no route holds, so mail to it takes the unresolved egress.
const UNROUTED: MailboxId = MailboxId(0x0a77_ac4e_0001);

const SHARED: &[u8] = b"shared bytes";

#[aether_data::kind(name = "test.attachments.carrier")]
struct Carrier {
    note: String,
    blob: Blob,
}

fn register_carrier(registry: &Registry) {
    registry
        .register_kind_with_descriptor(
            &boot_authority(),
            KindDescriptor { name: Carrier::NAME.into(), schema: Carrier::SCHEMA },
        )
        .expect("register the carrier kind");
}

/// A carrier whose blob is `SHARED` checked into `mailer`'s engine store,
/// encoded as an attached in-process send writes it. Only the returned
/// attachments hold the entry.
fn attached_carrier(mailer: &Mailer, note: String) -> (Vec<u8>, Attachments) {
    let blob = mailer.blob_store().check_in(Box::from(SHARED)).into_blob();
    SharingEncoder::encode(&Carrier { note, blob })
}

fn unresolved_payloads(rx: &Receiver<EgressEvent>) -> Vec<Vec<u8>> {
    rx.try_iter()
        .filter_map(|event| match event {
            EgressEvent::UnresolvedMail { payload, .. } => Some(payload),
            _ => None,
        })
        .collect()
}

fn decoded(payload: &[u8]) -> Value {
    decode_schema(payload, &Carrier::SCHEMA).expect("a tag-0 carrier payload decodes")
}

/// The inbox hand-off moves the attachments onto the dispatch, and they keep
/// the entry resident on their own until the dispatch drops. Catches a
/// hand-off that drops the field (the entry would go early) or leaks it (it
/// would never go).
#[test]
fn inbox_dispatch_holds_the_entries_until_it_drops() {
    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel::<OwnedDispatch>();
    let recipient = registry.register_inbox(
        &boot_authority(),
        "test.attachments.inbox",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    let (payload, attachments) = attached_carrier(&mailer, "inbox".into());

    mailer.push(Mail::new(recipient, Carrier::ID, payload, 1).with_attachments(attachments));
    let dispatch = rx.try_recv().expect("the inbox receives the mail");

    assert_eq!(dispatch.attachments().len(), 1);
    assert_eq!(mailer.blob_store().resident_bytes(), SHARED.len(), "the dispatch alone keeps the entry");
    drop(dispatch);
    assert_eq!(mailer.blob_store().resident_bytes(), 0, "the entry goes with the last holder");
}

/// An inline mailbox reads plain bytes, so it is lent the rewritten payload.
/// Catches an inline arm that lends the tag-1 bytes as they are.
#[test]
fn inline_mailbox_is_lent_inline_bytes() {
    let (registry, mailer) = bare_substrate();
    register_carrier(&registry);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let recipient = registry.register_inline(
        &boot_authority(),
        "test.attachments.inline",
        Arc::new(move |dispatch: MailDispatch<'_>| {
            sink.lock().expect("sink lock").push(dispatch.payload.to_vec());
        }),
    );
    let (payload, attachments) = attached_carrier(&mailer, "inline".into());

    mailer.push(Mail::new(recipient.id(), Carrier::ID, payload, 1).with_attachments(attachments));

    let seen = mem::take(&mut *seen.lock().expect("sink lock"));
    assert_eq!(seen.len(), 1, "the inline handler ran once");
    assert_eq!(decoded(&seen[0]), json!({ "note": "inline", "blob": SHARED }));
}

/// Mail bubbling out to the hub leaves as tag-0 bytes carrying the
/// attachment's contents. Catches a tag-1 payload leaving the process.
#[test]
fn unresolved_egress_writes_inline_bytes() {
    let (mailer, rx) = test_mailer_and_rx();
    register_carrier(mailer.registry());
    let (payload, attachments) = attached_carrier(&mailer, "egress".into());

    mailer.push(Mail::new(UNROUTED, Carrier::ID, payload, 1).with_attachments(attachments));

    let payloads = unresolved_payloads(&rx);
    assert_eq!(payloads.len(), 1, "the mail leaves once");
    assert_eq!(decoded(&payloads[0]), json!({ "note": "egress", "blob": SHARED }));
}

/// An attached mail whose inline form exceeds one frame does not leave at
/// all. Catches an egress that ignores the frame limit.
#[test]
fn unresolved_egress_refuses_a_payload_over_the_frame_limit() {
    let (mailer, rx) = test_mailer_and_rx();
    register_carrier(mailer.registry());
    let (payload, attachments) = attached_carrier(&mailer, "x".repeat(max_frame_size()));

    mailer.push(Mail::new(UNROUTED, Carrier::ID, payload, 1).with_attachments(attachments));

    assert!(unresolved_payloads(&rx).is_empty(), "an over-limit payload must not leave the process");
}
