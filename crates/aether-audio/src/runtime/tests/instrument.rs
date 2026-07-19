use super::*;

// ADR-0103 sampled instrument banks (#1679). The synth-side tests
// drive `Synth` directly (registry + sample-voice kernel); the
// cap-handler tests drive `on_load_instrument` / `on_read_result` /
// `on_instrument_assembled` through a `new_for_test` binding, the
// same pattern as the track tests above.

fn test_region(lokey: u8, hikey: u8, lovel: u8, hivel: u8, pitch_keycenter: u8, pcm: Vec<f32>) -> SampleRegion {
    SampleRegion { lokey, hikey, lovel, hivel, pitch_keycenter, pcm: Arc::from(pcm), loop_region: None }
}

/// A full-range region carrying a device-rate sustain loop over
/// `[start, end)`, for the sample-voice loop tests.
fn looped_region(pcm: Vec<f32>, start: f32, end: f32) -> SampleRegion {
    SampleRegion {
        lokey: 0,
        hikey: 127,
        lovel: 0,
        hivel: 127,
        pitch_keycenter: 60,
        pcm: Arc::from(pcm),
        loop_region: Some(SampleLoop { start, end }),
    }
}

fn test_bank(regions: Vec<SampleRegion>) -> Arc<SampleBank> {
    let resident_bytes = regions.iter().map(|r| r.pcm.len() * 4).sum();
    Arc::new(SampleBank { name: "test".to_owned(), regions, resident_bytes })
}

#[test]
fn loaded_bank_registers_past_builtins_and_plays() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    let id = builtin_id_ceiling();
    sender
        .push(AudioEvent::RegisterInstrument { id, bank: test_bank(vec![test_region(0, 127, 0, 127, 60, ramp(256))]) })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.bank_count(), 1, "bank not appended past the built-ins");

    sender
        .push(AudioEvent::NoteOn { sender_mailbox: MailboxId(1), pitch: 60, velocity: 100, instrument_id: id, pan: 0 })
        .unwrap();
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1, "loaded id did not sound a voice");
    assert!(buf.iter().any(|s| s.abs() > 0.0), "sampled instrument produced silence");
}

#[test]
fn banks_register_in_load_order() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    let first = builtin_id_ceiling();
    let second = first + 1;
    sender
        .push(AudioEvent::RegisterInstrument {
            id: first,
            bank: test_bank(vec![test_region(60, 60, 0, 127, 60, ramp(64))]),
        })
        .unwrap();
    sender
        .push(AudioEvent::RegisterInstrument {
            id: second,
            bank: test_bank(vec![test_region(72, 72, 0, 127, 72, ramp(64))]),
        })
        .unwrap();
    let mut buf = vec![0.0f32; 32];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.bank_count(), 2);
    assert!(synth.bank_for(first).unwrap().select(60, 100).is_some(), "id {first} should resolve the first bank");
    assert!(synth.bank_for(second).unwrap().select(72, 100).is_some(), "id {second} should resolve the second bank");
}

#[test]
fn note_on_unknown_loaded_id_drops() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // An id past the built-ins with no bank registered: no voice.
    sender
        .push(AudioEvent::NoteOn {
            sender_mailbox: MailboxId(1),
            pitch: 60,
            velocity: 100,
            instrument_id: builtin_id_ceiling() + 5,
            pan: 0,
        })
        .unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 0);
}

#[test]
fn note_on_outside_every_region_drops() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender
        .push(AudioEvent::RegisterInstrument {
            id: builtin_id_ceiling(),
            bank: test_bank(vec![test_region(60, 60, 0, 127, 60, ramp(64))]),
        })
        .unwrap();
    let mut buf = vec![0.0f32; 32];
    synth.fill(&mut buf, 1);
    // Pitch 30 falls outside the bank's only region.
    sender
        .push(AudioEvent::NoteOn {
            sender_mailbox: MailboxId(1),
            pitch: 30,
            velocity: 100,
            instrument_id: builtin_id_ceiling(),
            pan: 0,
        })
        .unwrap();
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 0, "note in an uncovered gap must drop");
}

#[test]
fn region_selected_by_pitch_and_velocity() {
    let bank = test_bank(vec![test_region(60, 71, 0, 63, 60, ramp(8)), test_region(60, 71, 64, 127, 60, ramp(8))]);
    let soft = bank.select(64, 30).expect("soft region covers low velocity");
    let loud = bank.select(64, 110).expect("loud region covers high velocity");
    assert_eq!((soft.lovel, soft.hivel), (0, 63));
    assert_eq!((loud.lovel, loud.hivel), (64, 127));
    assert!(bank.select(90, 100).is_none(), "pitch above every region");
}

#[test]
fn sample_voice_ends_when_sample_exhausts() {
    // At pitch == pitch_keycenter the rate ratio is 1.0, so the
    // unlooped voice walks one PCM sample per output sample and ends
    // when the 480-sample recording runs out (ADR-0103 §6).
    let region = test_region(60, 60, 0, 127, 60, ramp(480));
    let mut voice = SampleVoice::new(60, 100, &region);
    let dt = 1.0 / TEST_RATE;
    let mut n: usize = 0;
    while !voice.done() && n < 10_000 {
        voice.next_sample(dt);
        n += 1;
    }
    assert!(voice.done(), "sample voice never finished");
    assert!((479..=481).contains(&n), "ended at {n} samples, expected ~480");
}

#[test]
fn note_off_release_ends_sample_voice_before_sample_end() {
    // A one-second recording, released early: the 0.08s release ramp
    // ends the voice far short of the sample's natural end.
    let region = test_region(60, 60, 0, 127, 60, ramp(48_000));
    let mut voice = SampleVoice::new(60, 100, &region);
    let dt = 1.0 / TEST_RATE;
    for _ in 0..480 {
        voice.next_sample(dt);
    }
    voice.note_off();
    let mut n: usize = 480;
    while !voice.done() && n < 48_000 {
        voice.next_sample(dt);
        n += 1;
    }
    assert!(voice.done(), "released sample voice never ended");
    assert!(n < 10_000, "release ({n}) should end well before the sample exhausts");
}

#[test]
fn looped_sample_voice_sustains_past_sample_length() {
    // A 480-sample recording with a sustain loop holds the note far
    // past its own length: the voice cycles the loop region rather
    // than exhausting (ADR-0103 §6).
    let region = looped_region(ramp(480), 100.0, 400.0);
    let mut voice = SampleVoice::new(60, 100, &region);
    let dt = 1.0 / TEST_RATE;
    // Render past 2x the sample length while the key is held.
    let mut sounded = false;
    for _ in 0..1200 {
        if voice.next_sample(dt).abs() > 0.0 {
            sounded = true;
        }
    }
    assert!(!voice.done(), "held looped voice ended at sample exhaustion");
    assert!(sounded, "held looped voice produced silence");
}

#[test]
fn looped_sample_voice_ends_on_note_off_release() {
    // The loop holds the note open; note_off arms the release ramp,
    // which retires the voice while the loop keeps cycling beneath
    // it (ADR-0103 §6).
    let region = looped_region(ramp(480), 100.0, 400.0);
    let mut voice = SampleVoice::new(60, 100, &region);
    let dt = 1.0 / TEST_RATE;
    for _ in 0..2000 {
        voice.next_sample(dt);
    }
    assert!(!voice.done(), "voice should still be held before note_off");
    voice.note_off();
    let mut n = 0;
    while !voice.done() && n < 48_000 {
        voice.next_sample(dt);
        n += 1;
    }
    assert!(voice.done(), "released looped voice never ended");
    assert!(n < 10_000, "release ({n}) should retire the voice within the ramp");
}

#[test]
fn assemble_bank_scales_loop_points_by_resample_ratio() {
    // A source WAV at half the device rate resamples 2x at load, so
    // the source-frame loop offsets scale 2x into device-rate
    // positions (ADR-0103 §6).
    let region = SfzRegion {
        sample: "a.wav".to_owned(),
        lokey: 0,
        hikey: 127,
        lovel: 0,
        hivel: 127,
        pitch_keycenter: 60,
        loop_spec: Some(SfzLoop { start: 100, end: 400, mode: sfz::LoopMode::Continuous }),
    };
    let wav = decode::wav_int16_mono(&ramp(1000), 24_000);
    let bank =
        assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), wav)], 48_000).expect("bank assembles");
    let lp = bank.regions[0].loop_region.expect("loop scaled through to the region");
    assert!((lp.start - 200.0).abs() < 2.0, "loop_start should scale ~2x to 200, got {}", lp.start);
    assert!((lp.end - 800.0).abs() < 2.0, "loop_end should scale ~2x to 800, got {}", lp.end);
}

#[test]
fn assemble_bank_clamps_loop_end_to_resampled_length() {
    // A loop_end past the sample clamps to the resampled length
    // rather than reading out of bounds (ADR-0103 §6).
    let region = SfzRegion {
        sample: "a.wav".to_owned(),
        lokey: 0,
        hikey: 127,
        lovel: 0,
        hivel: 127,
        pitch_keycenter: 60,
        loop_spec: Some(SfzLoop { start: 10, end: 100_000, mode: sfz::LoopMode::Continuous }),
    };
    let wav = decode::wav_int16_mono(&ramp(1000), 24_000);
    let bank =
        assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), wav)], 48_000).expect("bank assembles");
    let region = &bank.regions[0];
    let lp = region.loop_region.expect("loop scaled through");
    #[allow(clippy::cast_precision_loss)]
    let len = region.pcm.len() as f32;
    assert!(lp.end <= len, "loop_end {} must clamp to the resampled length {len}", lp.end);
}

#[test]
fn unlooped_region_assembles_without_a_loop() {
    // A region with no loop_spec stays unlooped through assembly
    // (the piano-class regression path).
    let region = SfzRegion {
        sample: "a.wav".to_owned(),
        lokey: 0,
        hikey: 127,
        lovel: 0,
        hivel: 127,
        pitch_keycenter: 60,
        loop_spec: None,
    };
    let wav = decode::wav_int16_mono(&ramp(256), 24_000);
    let bank =
        assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), wav)], 48_000).expect("bank assembles");
    assert_eq!(bank.regions[0].loop_region, None);
}

#[test]
fn sample_voices_count_against_max_voices() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    sender
        .push(AudioEvent::RegisterInstrument {
            id: builtin_id_ceiling(),
            bank: test_bank(vec![test_region(0, 127, 0, 127, 60, ramp(48_000))]),
        })
        .unwrap();
    let mut buf = vec![0.0f32; 32];
    synth.fill(&mut buf, 1);
    // Saturate the pool with sampled voices: they steal like any other.
    for i in 0..(MAX_VOICES as u64 + 8) {
        sender
            .push(AudioEvent::NoteOn {
                sender_mailbox: MailboxId(i + 1),
                pitch: 60,
                velocity: 100,
                instrument_id: builtin_id_ceiling(),
                pan: 0,
            })
            .unwrap();
    }
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), MAX_VOICES, "sample voices must count against MAX_VOICES and steal");
}

#[test]
fn load_instrument_happy_path_replies_ok_and_registers() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));

    let mut ctx = manual_ctx(&transport);
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "piano/bank.sfz".to_owned() },
    );
    let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    // The .sfz parses into two regions referencing two samples.
    let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
";
    let mut read_ctx = read_result_ctx(&transport, sfz_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut read_ctx,
        ReadResult::Ok {
            namespace: "assets".to_owned(),
            path: "piano/bank.sfz".to_owned(),
            bytes: sfz.as_bytes().to_vec(),
        },
    );
    assert_eq!(cap.assemblies.len(), 1, "assembly not parked");
    let c4_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let c5_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    // Half the device rate, so decode also resamples each sample.
    let wav = decode::wav_int16_mono(&ramp(256), 24_000);
    let mut c4_ctx = read_result_ctx(&transport, c4_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut c4_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/c4.wav".to_owned(), bytes: wav.clone() },
    );
    // One sample still missing — no dispatch yet.
    assert_eq!(cap.assemblies.len(), 1, "assembly dispatched too early");
    let mut c5_ctx = read_result_ctx(&transport, c5_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut c5_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/c5.wav".to_owned(), bytes: wav },
    );
    // The last sample triggers the assembly dispatch off-thread.
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);

    match decode_session_reply::<LoadInstrumentResult>(&rx) {
        LoadInstrumentResult::Ok { instrument_id, name, resident_bytes } => {
            assert_eq!(instrument_id, builtin_id_ceiling());
            assert_eq!(name, "bank");
            assert!(resident_bytes > 0, "resident bytes not reported");
        }
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    }
    assert!(cap.assemblies.is_empty(), "assembly never cleared");
    let event = queue.pop().expect("a register-instrument event was queued");
    assert!(
        matches!(event, AudioEvent::RegisterInstrument { id, .. } if id == builtin_id_ceiling()),
        "expected RegisterInstrument, got {event:?}",
    );
}

#[test]
fn same_wav_path_bank_loads_fill_their_own_sample_slots() {
    let (mut cap, _queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let first_session = SessionToken(Uuid::from_u128(1));
    let second_session = SessionToken(Uuid::from_u128(2));

    let mut first_ctx = NativeCtx::new_dispatching(
        &transport,
        Source::to(SourceAddr::Session(first_session)),
        MailId::NONE,
        MailId::NONE,
    );
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut first_ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "piano/bank_a.sfz".to_owned() },
    );
    let first_sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let mut second_ctx = NativeCtx::new_dispatching(
        &transport,
        Source::to(SourceAddr::Session(second_session)),
        MailId::NONE,
        MailId::NONE,
    );
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut second_ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "piano/bank_b.sfz".to_owned() },
    );
    let second_sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let sfz = b"<region>\nsample=shared.wav pitch_keycenter=60\n";
    let mut first_sfz_ctx = read_result_ctx(&transport, first_sfz_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut first_sfz_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/bank_a.sfz".to_owned(), bytes: sfz.to_vec() },
    );
    let first_sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let mut second_sfz_ctx = read_result_ctx(&transport, second_sfz_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut second_sfz_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/bank_b.sfz".to_owned(), bytes: sfz.to_vec() },
    );
    let second_sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let wav = decode::wav_int16_mono(&ramp(256), 24_000);
    let mut second_sample_ctx = read_result_ctx(&transport, second_sample_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut second_sample_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/shared.wav".to_owned(), bytes: wav.clone() },
    );
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
    let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
    assert_eq!(session, second_session);
    match reply {
        LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, "bank_b"),
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok: {error}"),
    }

    let mut first_sample_ctx = read_result_ctx(&transport, first_sample_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut first_sample_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/shared.wav".to_owned(), bytes: wav },
    );
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
    let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
    assert_eq!(session, first_session);
    match reply {
        LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, "bank_a"),
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok: {error}"),
    }
}

#[test]
fn interleaved_track_and_instrument_reads_demux_by_request_context() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let track_session = SessionToken(Uuid::from_u128(3));
    let instrument_session = SessionToken(Uuid::from_u128(4));

    let mut track_ctx = NativeCtx::new_dispatching(
        &transport,
        Source::to(SourceAddr::Session(track_session)),
        MailId::NONE,
        MailId::NONE,
    );
    AudioCapability::on_play_track(
        &mut cap,
        &mut track_ctx,
        PlayTrack {
            namespace: "assets".to_owned(),
            path: "piano/shared.wav".to_owned(),
            gain: 1.0,
            looping: false,
            lane: Some("intro".to_owned()),
        },
    );
    let track_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let mut instrument_ctx = NativeCtx::new_dispatching(
        &transport,
        Source::to(SourceAddr::Session(instrument_session)),
        MailId::NONE,
        MailId::NONE,
    );
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut instrument_ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "piano/bank.sfz".to_owned() },
    );
    let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut sfz_ctx,
        ReadResult::Ok {
            namespace: "assets".to_owned(),
            path: "piano/bank.sfz".to_owned(),
            bytes: b"<region>\nsample=shared.wav pitch_keycenter=60\n".to_vec(),
        },
    );
    let sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);

    let wav = decode::wav_int16_mono(&ramp(256), 24_000);
    let mut sample_ctx = read_result_ctx(&transport, sample_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut sample_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/shared.wav".to_owned(), bytes: wav.clone() },
    );
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
    let (session, reply) = decode_session_reply_with_session::<LoadInstrumentResult>(&rx);
    assert_eq!(session, instrument_session);
    assert!(matches!(reply, LoadInstrumentResult::Ok { .. }), "sample reply should complete the instrument load");

    let mut track_read_ctx = read_result_ctx(&transport, track_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut track_read_ctx,
        ReadResult::Ok { namespace: "assets".to_owned(), path: "piano/shared.wav".to_owned(), bytes: wav },
    );
    drive_task_completion::<AudioCapability>(&mut cap, &transport, &rx);
    let (session, reply) = decode_session_reply_with_session::<PlayTrackResult>(&rx);
    assert_eq!(session, track_session);
    match reply {
        PlayTrackResult::Ok { lane, .. } => assert_eq!(lane.as_deref(), Some("intro")),
        PlayTrackResult::Err { error, .. } => panic!("expected Ok: {error}"),
    }

    assert!(
        matches!(queue.pop(), Some(AudioEvent::RegisterInstrument { .. })),
        "instrument load should register a bank"
    );
    assert!(matches!(queue.pop(), Some(AudioEvent::TrackStart { .. })), "track load should start a track");
}

#[test]
fn load_instrument_missing_sample_replies_err() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = manual_ctx(&transport);
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "bank.sfz".to_owned() },
    );
    let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut sfz_ctx,
        ReadResult::Ok {
            namespace: "assets".to_owned(),
            path: "bank.sfz".to_owned(),
            bytes: b"<region>\nsample=c4.wav\n".to_vec(),
        },
    );
    let sample_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    // The bank's only sample fails to read — the whole load fails.
    let mut sample_ctx = read_result_ctx(&transport, sample_correlation);
    AudioCapability::on_read_result(
        &mut cap,
        &mut sample_ctx,
        ReadResult::Err { namespace: "assets".to_owned(), path: "c4.wav".to_owned(), error: FsError::NotFound },
    );
    match decode_session_reply::<LoadInstrumentResult>(&rx) {
        LoadInstrumentResult::Err { error, .. } => {
            assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
        }
        LoadInstrumentResult::Ok { .. } => panic!("expected Err for a missing sample"),
    }
    assert!(cap.assemblies.is_empty(), "assembly never discarded");
    assert!(queue.pop().is_none(), "a failed bank must not register");
}

#[test]
fn load_instrument_malformed_sfz_replies_err() {
    let (mut cap, queue) = live_cap();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = manual_ctx(&transport);
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "bank.sfz".to_owned() },
    );
    let sfz_correlation = assert_next_send_kind::<Read>(&transport, &rx);
    let mut sfz_ctx = read_result_ctx(&transport, sfz_correlation);
    // A control block with no regions: the parser rejects it.
    AudioCapability::on_read_result(
        &mut cap,
        &mut sfz_ctx,
        ReadResult::Ok {
            namespace: "assets".to_owned(),
            path: "bank.sfz".to_owned(),
            bytes: b"<control>\ndefault_path=x/\n".to_vec(),
        },
    );
    match decode_session_reply::<LoadInstrumentResult>(&rx) {
        LoadInstrumentResult::Err { error, .. } => {
            assert!(error.contains("parse"), "parse error not surfaced: {error}");
        }
        LoadInstrumentResult::Ok { .. } => panic!("expected Err for malformed sfz"),
    }
    assert!(cap.assemblies.is_empty(), "no assembly should be parked");
    assert!(queue.pop().is_none(), "a malformed bank must not register");
}

#[test]
fn load_instrument_on_nop_chassis_replies_err() {
    let mut cap = AudioCapabilityState::nop();
    let (mailer, rx) = test_mailer_and_rx();
    let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
    let mut ctx = manual_ctx(&transport);
    AudioCapability::on_load_instrument(
        &mut cap,
        &mut ctx,
        LoadInstrument { namespace: "assets".to_owned(), path: "bank.sfz".to_owned() },
    );
    match decode_session_reply::<LoadInstrumentResult>(&rx) {
        LoadInstrumentResult::Err { .. } => {}
        LoadInstrumentResult::Ok { .. } => panic!("nop chassis must reply Err"),
    }
    assert!(rx.try_recv().is_err(), "nop chassis must not forward a read");
}
