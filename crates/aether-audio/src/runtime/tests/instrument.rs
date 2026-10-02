use super::*;
use aether_data::Blob;

// ADR-0103 sampled instrument banks (#1679). These drive `Synth` directly
// (registry + sample-voice kernel) and the bank assembly; the cap's
// handlers are covered by `tests/handlers.rs`.

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

    sender.push(AudioEvent::NoteOn { sender: None, pitch: 60, velocity: 100, instrument_id: id, pan: 0 }).unwrap();
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
            sender: None,
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
            sender: None,
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
    let bank = assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), Blob::from(wav))], 48_000)
        .expect("bank assembles");
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
    let bank = assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), Blob::from(wav))], 48_000)
        .expect("bank assembles");
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
    let bank = assemble_bank("test".to_owned(), &[region], &[("a.wav".to_owned(), Blob::from(wav))], 48_000)
        .expect("bank assembles");
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
    for _ in 0..MAX_VOICES + 8 {
        sender
            .push(AudioEvent::NoteOn {
                sender: None,
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
