use super::*;

/// Substrate with a registry, settlement counter, egress rx (for
/// `drive_task_completion`), and a registered component inbox.
///
/// The inbox handler discharges the ADR-0094 obligation before
/// forwarding so the caller can observe the `OwnedDispatch` (and
/// call `record_finished`) without tripping the debug guard on drop.
///
/// Returns `(mailer, egress_rx, caller_mailbox, reply_rx)`.
fn settlement_substrate() -> (Arc<Mailer>, mpsc::Receiver<EgressEvent>, MailboxId, mpsc::Receiver<OwnedDispatch>) {
    let reg = Arc::new(Registry::new());
    let (outbound, egress_rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&reg)).with_outbound(outbound));
    let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
    let caller_mailbox = reg.register_inbox(
        "test.audio.settlement.caller",
        Arc::new(move |dispatch: OwnedDispatch| {
            // ADR-0094: terminal consumer — discharge before forwarding.
            dispatch.discharge();
            let _ = reply_tx.send(dispatch);
        }) as Arc<dyn InboxHandler>,
    );
    (mailer, egress_rx, caller_mailbox, reply_rx)
}
/// #1693 / #1701 regression: a deferred `play_track` reply
/// (read → decode worker → resolve) must inherit the caller's
/// root and keep the chain UNSETTLED (`live_roots == 1`) until
/// the reply's `Finished` fires; `live_roots == 0` after.
///
/// Before the fix the reply carried `MailId::NONE` as root, so
/// `record_sent_inflight` was a no-op and the chain settled
/// prematurely (caller's settlement window closed too early).
#[test]
fn play_track_deferred_reply_settles_caller_chain() {
    let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let (mut cap, _queue) = live_cap();
    let root = MailId::new(MailboxId(0xC0), 1);
    let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 1);

    {
        let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
        AudioCapability::on_play_track(
            &mut cap,
            &mut ctx,
            PlayTrack {
                namespace: "assets".to_owned(),
                path: "track.wav".to_owned(),
                gain: 0.8,
                looping: false,
                lane: None,
            },
        );
    }

    let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let wav = decode::wav_int16_mono(&ramp(512), 24_000);
    {
        let mut read_ctx = NativeCtx::new_dispatching(&transport, fs_reply_source(track_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { namespace: "assets".to_owned(), path: "track.wav".to_owned(), bytes: wav },
        );
    }

    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    // The settlement hold was released inside resolve_with, but the
    // reply is now in-flight on the caller root — live_roots must
    // stay at 1. Pre-fix: root was MailId::NONE so record_sent_inflight
    // was a no-op and live_roots dropped to 0 here (premature settle).
    assert_eq!(counter.live_roots(), 1, "deferred reply holds the caller chain open after hold releases");

    let dispatch = reply_rx.recv_timeout(Duration::from_secs(2)).expect("reply reached the caller inbox");
    assert_eq!(dispatch.root, root, "reply inherits the caller's root");
    mailer.record_finished(dispatch.mail_id, dispatch.root);
    assert_eq!(counter.live_roots(), 0, "chain settles after the reply's Finished fires");
}

/// #1693 / #1701 regression: `load_instrument`'s deferred assembly
/// reply (sfz.read → sample reads → assembly dispatch → resolve)
/// must keep the chain UNSETTLED until the reply's `Finished` fires.
#[test]
fn load_instrument_deferred_reply_settles_caller_chain() {
    let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let (mut cap, _queue) = live_cap();
    let root = MailId::new(MailboxId(0xC0), 3);
    let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 3);

    {
        let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
        AudioCapability::on_load_instrument(
            &mut cap,
            &mut ctx,
            LoadInstrument { namespace: "assets".to_owned(), path: "piano/bank.sfz".to_owned() },
        );
    }

    let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
    ";
    let wav = decode::wav_int16_mono(&ramp(256), 24_000);
    {
        let mut read_ctx = NativeCtx::new_dispatching(&transport, fs_reply_source(sfz_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "piano/bank.sfz".to_owned(),
                bytes: sfz.as_bytes().to_vec(),
            },
        );
    }
    let c4_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let c5_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    {
        let mut read_ctx = NativeCtx::new_dispatching(&transport, fs_reply_source(c4_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/c4.wav".to_owned(), bytes: wav.clone() },
        );
    }
    {
        // Last sample — triggers assembly dispatch and hold acquisition.
        let mut read_ctx = NativeCtx::new_dispatching(&transport, fs_reply_source(c5_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/c5.wav".to_owned(), bytes: wav },
        );
    }

    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    assert_eq!(counter.live_roots(), 1, "assembly reply holds the caller chain open after hold releases");

    let dispatch = reply_rx.recv_timeout(Duration::from_secs(2)).expect("reply reached the caller inbox");
    assert_eq!(dispatch.root, root, "assembly reply inherits the caller's root");
    mailer.record_finished(dispatch.mail_id, dispatch.root);
    assert_eq!(counter.live_roots(), 0, "chain settles after the reply's Finished fires");
}
