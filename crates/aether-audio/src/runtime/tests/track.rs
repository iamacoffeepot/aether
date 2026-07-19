use super::*;

// ADR-0103 track lane. The synth-side tests drive `Synth` directly
// (the same pattern as the note tests); the cap-handler tests drive
// the `on_play_track` / `on_read_result` / `on_track_decoded` /
// `on_stop_track` arms through a `new_for_test` binding.

/// A short ramp track at the device rate — long enough to span a
/// few `fill` blocks but cheap to play to completion.
fn ramp_pcm(len: usize) -> Arc<[f32]> {
    // Index-to-float over a small range — exact in f32.
    #[allow(clippy::cast_precision_loss)]
    let v: Vec<f32> = (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect();
    Arc::from(v)
}

fn track_start(pcm: Arc<[f32]>, looping: bool) -> AudioEvent {
    AudioEvent::TrackStart {
        sender_mailbox: MailboxId(1),
        lane: None,
        namespace: "assets".to_owned(),
        path: "track.wav".to_owned(),
        pcm,
        gain: 1.0,
        looping,
    }
}

#[test]
fn track_plays_to_completion_then_retires() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender.push(track_start(ramp_pcm(256), false)).unwrap();
    let mut buf = vec![0.0f32; 64];
    // First block starts the track and produces sound.
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 1);
    assert!(buf.iter().any(|s| s.abs() > 0.0), "track produced silence");
    // 256 samples / 64-sample blocks: a few more blocks retire it.
    for _ in 0..8 {
        synth.fill(&mut buf, 1);
    }
    assert_eq!(synth.track_count(), 0, "finished track never retired");
}

#[test]
fn looping_track_outlives_its_length() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender.push(track_start(ramp_pcm(128), true)).unwrap();
    let mut buf = vec![0.0f32; 128];
    // Play well past the PCM length — a looping track wraps rather
    // than retiring.
    for _ in 0..10 {
        synth.fill(&mut buf, 1);
    }
    assert_eq!(synth.track_count(), 1, "looping track retired early");
}

#[test]
fn stop_track_fades_then_retires() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender.push(track_start(ramp_pcm(4_800), true)).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 1);
    // Stop, then fill past the ~5ms fade window (240 samples at
    // 48kHz): the track fades out and retires.
    sender
        .push(AudioEvent::TrackStop {
            sender_mailbox: MailboxId(1),
            lane: None,
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
        })
        .unwrap();
    let mut tail = vec![0.0f32; 512];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.track_count(), 0, "stopped track never retired");
}

#[test]
fn track_does_not_count_against_max_voices() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // Saturate the voice pool.
    for i in 0..(MAX_VOICES as u64 + 8) {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(i + 1),
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
                pan: 0,
            })
            .unwrap();
    }
    // A track plays alongside without being stolen or counted.
    sender.push(track_start(ramp_pcm(4_800), true)).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), MAX_VOICES, "voice cap shifted");
    assert_eq!(synth.track_count(), 1, "track not playing in its own lane");
}

#[test]
fn replay_same_key_restarts_single_track() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    for _ in 0..3 {
        sender.push(track_start(ramp_pcm(256), true)).unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 1, "re-playing the same key must restart, not stack");
}

/// A `TrackStart` at an explicit sender + lane over the shared
/// `(namespace, path)` — the key components the collision fix
/// folds together.
fn keyed_track_start(sender_mailbox: MailboxId, lane: Option<&str>, pcm: Arc<[f32]>) -> AudioEvent {
    AudioEvent::TrackStart {
        sender_mailbox,
        lane: lane.map(str::to_owned),
        namespace: "assets".to_owned(),
        path: "track.wav".to_owned(),
        pcm,
        gain: 1.0,
        looping: true,
    }
}

#[test]
fn distinct_lanes_under_one_sender_play_independently() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // Two senders that collapse to the same MailboxId(0) (MCP
    // sessions) play the same path under distinct lanes.
    sender.push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(4_800))).unwrap();
    sender.push(keyed_track_start(MailboxId(0), Some("b"), ramp_pcm(4_800))).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 2, "distinct lanes must not alias to one track");
    // Stopping lane a leaves lane b sounding.
    sender
        .push(AudioEvent::TrackStop {
            sender_mailbox: MailboxId(0),
            lane: Some("a".to_owned()),
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
        })
        .unwrap();
    let mut tail = vec![0.0f32; 512];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.track_count(), 1, "stopping one lane must not silence the other");
}

#[test]
fn same_sender_and_lane_replays_single_track() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    for _ in 0..3 {
        sender.push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(256))).unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 1, "re-playing the same (sender, lane) key must restart, not stack");
}
#[test]
fn play_track_happy_path_replies_ok_and_starts_a_track() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));

    let root = MailId::new(MailboxId(0xC0), 1);
    let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), root, root);
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
    // The cap forwarded an fs.read with a request context.
    let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    // Synthesize the fs reply with a real WAV asset (at half the
    // device rate, so decode also resamples).
    let wav = decode::wav_int16_mono(&ramp(512), 24_000);
    let mut read_ctx = NativeCtx::new_dispatching(&transport, fs_reply_source(track_correlation), root, root);
    AudioCapability::on_read_result(
        &mut cap,
        &mut read_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "track.wav".to_owned(), bytes: wav },
    );
    // The decode worker runs off-thread and pushes the completion
    // wake; route it through the cap's #[handler(task)] arm.
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    match decode_session_reply::<PlayTrackResult>(&rx) {
        PlayTrackResult::Ok { namespace, path, lane } => {
            assert_eq!(namespace, "assets");
            assert_eq!(path, "track.wav");
            assert_eq!(lane, None);
        }
        PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    }
    // The decoded track reached the synth queue as a TrackStart.
    let event = queue.pop().expect("a track-start event was queued");
    assert!(
        matches!(event, AudioEvent::TrackStart { ref path, .. } if path == "track.wav"),
        "expected TrackStart, got {event:?}",
    );
}

#[test]
fn play_track_echoes_lane_through_result_and_track_start() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));

    let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
    AudioCapability::on_play_track(
        &mut cap,
        &mut ctx,
        PlayTrack {
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
            gain: 1.0,
            looping: false,
            lane: Some("bgm".to_owned()),
        },
    );
    let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let wav = decode::wav_int16_mono(&ramp(512), 24_000);
    let mut read_ctx = read_result_ctx(&transport, track_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut read_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "track.wav".to_owned(), bytes: wav },
    );
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    match decode_session_reply::<PlayTrackResult>(&rx) {
        PlayTrackResult::Ok { lane, .. } => {
            assert_eq!(lane, Some("bgm".to_owned()), "result must echo the lane");
        }
        PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    }
    let event = queue.pop().expect("a track-start event was queued");
    assert!(
        matches!(event, AudioEvent::TrackStart { ref lane, .. } if lane.as_deref() == Some("bgm")),
        "TrackStart must carry the lane, got {event:?}",
    );
}

#[test]
fn play_track_missing_file_replies_err_with_fs_error() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));

    let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
    AudioCapability::on_play_track(
        &mut cap,
        &mut ctx,
        PlayTrack {
            namespace: "assets".to_owned(),
            path: "missing.wav".to_owned(),
            gain: 1.0,
            looping: false,
            lane: None,
        },
    );
    let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let mut read_ctx = read_result_ctx(&transport, track_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut read_ctx,
        ReadResult::Err { namespace: "assets".to_owned(), path: "missing.wav".to_owned(), error: FsError::NotFound },
    );

    match decode_session_reply::<PlayTrackResult>(&rx) {
        PlayTrackResult::Err { path, error, .. } => {
            assert_eq!(path, "missing.wav");
            assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
        }
        PlayTrackResult::Ok { .. } => panic!("expected Err for a missing file"),
    }
    assert!(queue.pop().is_none(), "a failed read must not start a track");
}

#[test]
fn play_track_on_nop_chassis_replies_err() {
    let mut cap = AudioCapabilityState::nop();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
    AudioCapability::on_play_track(
        &mut cap,
        &mut ctx,
        PlayTrack {
            namespace: "assets".to_owned(),
            path: "track.wav".to_owned(),
            gain: 1.0,
            looping: false,
            lane: None,
        },
    );
    match decode_session_reply::<PlayTrackResult>(&rx) {
        PlayTrackResult::Err { .. } => {}
        PlayTrackResult::Ok { .. } => panic!("nop chassis must reply Err"),
    }
    assert!(rx.try_recv().is_err(), "nop chassis must not forward a read");
    // stop_track on a nop chassis is a silent no-op (no panic).
    AudioCapability::on_stop_track(
        &mut cap,
        ctx.as_single(),
        StopTrack { namespace: "assets".to_owned(), path: "track.wav".to_owned(), lane: None },
    );
}
