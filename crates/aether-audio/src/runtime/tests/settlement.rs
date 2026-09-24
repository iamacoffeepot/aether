use super::*;

/// A substrate with a settlement counter, the egress rx
/// `drive_task_completion` drains, and a registered caller whose reply
/// target and chain root are the stamp of a mail it really sent.
struct SettlementFixture {
    mailer: Arc<Mailer>,
    egress: mpsc::Receiver<EgressEvent>,
    caller: Source,
    root: MailId,
    replies: mpsc::Receiver<OwnedDispatch>,
}

/// Build a [`SettlementFixture`]. The caller's inbox discharges the
/// ADR-0094 obligation before forwarding each dispatch to `replies`, so
/// the test can observe the `OwnedDispatch` (and call `record_finished`)
/// without tripping the debug guard on drop.
///
/// The caller sends one mail to itself on a fresh chain. Its dispatch
/// carries the `Source` and root the binding's send path stamps, which
/// are what a real caller's request would hand the cap. The fixture
/// records that mail's `Finished`, so the settlement counter starts at
/// zero.
fn settlement_substrate() -> SettlementFixture {
    let reg = Arc::new(Registry::new());
    let (outbound, egress) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&reg)).with_outbound(outbound));
    let (reply_tx, replies) = mpsc::channel::<OwnedDispatch>();
    let handler = Arc::new(move |dispatch: OwnedDispatch| {
        // ADR-0094: terminal consumer — discharge before forwarding.
        dispatch.discharge();
        let _ = reply_tx.send(dispatch);
    }) as Arc<dyn InboxHandler>;
    let (caller, caller_ref) = registered_binding(&reg, &mailer, "test.audio.settlement.caller", handler);

    NativeCtx::new(&caller, Source::NONE, MailId::NONE, MailId::NONE).send_to(caller_ref, &SetMasterGain { gain: 1.0 });

    let request = replies.recv_timeout(Duration::from_secs(2)).expect("the caller's own mail reached its inbox");
    mailer.record_finished(request.mail_id, request.root);
    SettlementFixture { mailer, egress, caller: request.sender, root: request.root, replies }
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
    let SettlementFixture { mailer, egress: rx, caller, root, replies: reply_rx } = settlement_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let transport = unrouted_binding(&mailer);
    let (mut cap, _queue) = live_cap();

    {
        let mut ctx = NativeCtx::new_for_actor(&transport, caller, root, root);
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
        let mut read_ctx = NativeCtx::new_for_actor(&transport, fs_reply_source(track_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { addr: NamespaceAddr::new("assets", "track.wav"), bytes: wav },
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
    let SettlementFixture { mailer, egress: rx, caller, root, replies: reply_rx } = settlement_substrate();
    let counter = Arc::clone(mailer.trace_handle().settlement_counter());
    let transport = unrouted_binding(&mailer);
    let (mut cap, _queue) = live_cap();

    {
        let mut ctx = NativeCtx::new_for_actor(&transport, caller, root, root);
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
        let mut read_ctx = NativeCtx::new_for_actor(&transport, fs_reply_source(sfz_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { addr: NamespaceAddr::new("assets", "piano/bank.sfz"), bytes: sfz.as_bytes().to_vec() },
        );
    }
    let c4_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let c5_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    {
        let mut read_ctx = NativeCtx::new_for_actor(&transport, fs_reply_source(c4_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { addr: NamespaceAddr::new("assets", "piano/c4.wav"), bytes: wav.clone() },
        );
    }
    {
        // Last sample — triggers assembly dispatch and hold acquisition.
        let mut read_ctx = NativeCtx::new_for_actor(&transport, fs_reply_source(c5_correlation), root, root);
        AudioCapability::on_read_result(
            &mut cap,
            &mut read_ctx,
            ReadResult::Ok { addr: NamespaceAddr::new("assets", "piano/c5.wav"), bytes: wav },
        );
    }

    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    assert_eq!(counter.live_roots(), 1, "assembly reply holds the caller chain open after hold releases");

    let dispatch = reply_rx.recv_timeout(Duration::from_secs(2)).expect("reply reached the caller inbox");
    assert_eq!(dispatch.root, root, "assembly reply inherits the caller's root");
    mailer.record_finished(dispatch.mail_id, dispatch.root);
    assert_eq!(counter.live_roots(), 0, "chain settles after the reply's Finished fires");
}
