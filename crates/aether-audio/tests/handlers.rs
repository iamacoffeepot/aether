//! `AudioCapability`'s request handlers over a [`SubstrateHarness`]: the real
//! cap booted with a `null` output, so the synth thread and event queue run
//! without a device, and `aether.fs` reading fixtures from the test sandbox.
//! Every assertion is on a reply the caller receives.

use std::io::Cursor;

use aether_actor::{ActorRef, HandlesKind};
use aether_audio::{
    AudioCapability, AudioConfig, AudioOutput, LoadInstrument, LoadInstrumentResult, PlayTrack, PlayTrackResult,
    SCHEDULE_MAX_EVENTS, SCHEDULE_MAX_MILLIS, Schedule, ScheduleResult, ScheduledEvent, ScheduledNote, SetMasterGain,
    SetMasterGainResult, SetReverbSend, SetReverbSendResult,
};
use aether_data::Kind;
use aether_harness_substrate::SubstrateHarness;
use aether_harness_substrate::test_helpers::{init_save_sandbox, test_namespace_roots, write_fixture};

/// Boot `AudioCapability` with `output` beside `aether.fs` over the test
/// sandbox, whose `assets` namespace the fixtures are written into.
fn boot(output: AudioOutput) -> (SubstrateHarness, ActorRef<AudioCapability>) {
    let harness = SubstrateHarness::builder()
        .namespace_roots(test_namespace_roots(init_save_sandbox("audio-handlers")))
        .with_actor_configured::<AudioCapability>((), AudioConfig { output, requested_sample_rate: None })
        .build()
        .expect("boot audio");
    let audio = harness.actor_ref::<AudioCapability>();
    (harness, audio)
}

/// Send `mail` to the audio cap and wait for its reply.
fn request<K, R>(harness: &mut SubstrateHarness, audio: ActorRef<AudioCapability>, mail: &K) -> R
where
    K: Kind,
    R: Kind,
    AudioCapability: HandlesKind<K>,
{
    let pending = harness.send_deferred(audio, mail);
    harness.await_deferred(pending).expect("the audio cap replies")
}

/// A mono 16-bit WAV of `frames` ramp samples at 24 kHz, half the `null`
/// output's rate, so every decode also resamples.
fn wav(frames: usize) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 24_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut bytes = Vec::new();
    let mut writer = hound::WavWriter::new(Cursor::new(&mut bytes), spec).expect("open the WAV writer");
    for i in 0..frames {
        let step = i16::try_from(i % 512).expect("a ramp step fits i16");
        writer.write_sample((step - 256) * 32).expect("write a WAV sample");
    }
    writer.finalize().expect("finish the WAV");
    bytes
}

fn note_at(at_millis: u32) -> ScheduledEvent {
    ScheduledEvent { at_millis, event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 } }
}

fn play(path: &str, lane: Option<&str>) -> PlayTrack {
    PlayTrack {
        namespace: "assets".to_owned(),
        path: path.to_owned(),
        gain: 1.0,
        looping: false,
        lane: lane.map(str::to_owned),
    }
}

fn load(path: &str) -> LoadInstrument {
    LoadInstrument { namespace: "assets".to_owned(), path: path.to_owned() }
}

/// Catches a schedule validation check that is dropped or lets its batch
/// through: a valid batch is accepted in full, and an empty batch, one event
/// past the per-batch cap, and one event past the horizon each reply `Err`
/// naming the limit it broke.
#[test]
fn schedule_accepts_a_valid_batch_and_rejects_each_invalid_one() {
    let (mut harness, audio) = boot(AudioOutput::Null);

    let valid = Schedule {
        events: vec![
            note_at(0),
            ScheduledEvent { at_millis: 500, event: ScheduledNote::Off { pitch: 60, instrument_id: 0 } },
        ],
    };
    assert!(matches!(request(&mut harness, audio, &valid), ScheduleResult::Ok { accepted: 2 }));

    let empty = Schedule { events: vec![] };
    assert!(matches!(request(&mut harness, audio, &empty), ScheduleResult::Err { .. }));

    let over_cap = Schedule { events: vec![note_at(0); SCHEDULE_MAX_EVENTS + 1] };
    match request(&mut harness, audio, &over_cap) {
        ScheduleResult::Err { error } => assert!(error.contains("cap"), "reason: {error}"),
        ScheduleResult::Ok { .. } => panic!("an over-cap batch must reject"),
    }

    let over_horizon = Schedule { events: vec![note_at(0), note_at(SCHEDULE_MAX_MILLIS + 1)] };
    match request(&mut harness, audio, &over_horizon) {
        ScheduleResult::Err { error } => assert!(error.contains("horizon"), "reason: {error}"),
        ScheduleResult::Ok { .. } => panic!("an over-horizon batch must reject"),
    }
}

/// Catches a master-gain or reverb-send handler that stops clamping, or
/// replies with the requested value rather than the applied one.
#[test]
fn master_gain_and_reverb_send_reply_the_clamped_value() {
    let (mut harness, audio) = boot(AudioOutput::Null);

    match request(&mut harness, audio, &SetMasterGain { gain: 1.5 }) {
        SetMasterGainResult::Ok { applied_gain } => assert_eq!(applied_gain, 1.0),
        SetMasterGainResult::Err { error } => panic!("expected Ok, got Err({error})"),
    }
    match request(&mut harness, audio, &SetReverbSend { send: 1.5 }) {
        SetReverbSendResult::Ok { applied_send } => assert_eq!(applied_send, 1.0),
        SetReverbSendResult::Err { error } => panic!("expected Ok, got Err({error})"),
    }
}

/// ADR-0103 §7: a cap with no synth answers every request that needs one with
/// `Err`. Catches a handler that answers `Ok` with no pipeline, and a deferred
/// load that forwards its read instead of failing fast and so never answers.
#[test]
fn disabled_output_replies_err_to_every_request() {
    let (mut harness, audio) = boot(AudioOutput::Disabled);

    assert!(matches!(request(&mut harness, audio, &Schedule { events: vec![note_at(0)] }), ScheduleResult::Err { .. }));
    assert!(matches!(request(&mut harness, audio, &SetMasterGain { gain: 0.5 }), SetMasterGainResult::Err { .. }));
    assert!(matches!(request(&mut harness, audio, &SetReverbSend { send: 0.5 }), SetReverbSendResult::Err { .. }));
    assert!(matches!(request(&mut harness, audio, &play("track.wav", None)), PlayTrackResult::Err { .. }));
    assert!(matches!(request(&mut harness, audio, &load("bank.sfz")), LoadInstrumentResult::Err { .. }));
}

/// The read → decode → reply flow of `play_track`. Catches a decode
/// completion that never answers the held reply, and a reply that drops the
/// caller's lane.
#[test]
fn play_track_decodes_the_asset_and_echoes_the_lane() {
    let (mut harness, audio) = boot(AudioOutput::Null);
    let path = write_fixture("play-track.wav", &wav(512));

    match request(&mut harness, audio, &play(&path, Some("bgm"))) {
        PlayTrackResult::Ok { namespace, path: played, lane } => {
            assert_eq!((namespace.as_str(), played.as_str()), ("assets", path.as_str()));
            assert_eq!(lane.as_deref(), Some("bgm"), "the reply must echo the lane");
        }
        PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    }
}

/// Catches the read-error arm and the decode-error arm of `play_track`
/// answering `Ok`, or losing the fs error the caller needs.
#[test]
fn play_track_replies_err_for_a_missing_or_undecodable_asset() {
    let (mut harness, audio) = boot(AudioOutput::Null);

    match request(&mut harness, audio, &play("missing.wav", None)) {
        PlayTrackResult::Err { path, error, .. } => {
            assert_eq!(path, "missing.wav");
            assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
        }
        PlayTrackResult::Ok { .. } => panic!("expected Err for a missing file"),
    }

    let garbage = write_fixture("not-a-wav.wav", b"not a wav file");
    assert!(matches!(request(&mut harness, audio, &play(&garbage, None)), PlayTrackResult::Err { .. }));
}

/// The `.sfz` read → sample fan-out → assembly → reply flow of
/// `load_instrument`. Catches an assembly that never answers, a bank reported
/// with no resident bytes, and ids that do not advance per load (two banks
/// aliased onto one id).
#[test]
fn load_instrument_assembles_the_bank_and_assigns_increasing_ids() {
    let (mut harness, audio) = boot(AudioOutput::Null);
    write_fixture("load-c4.wav", &wav(256));
    write_fixture("load-c5.wav", &wav(256));
    let sfz = write_fixture(
        "load.sfz",
        b"<region>\nsample=load-c4.wav lokey=60 hikey=71 pitch_keycenter=60\n\
          <region>\nsample=load-c5.wav lokey=72 hikey=83 pitch_keycenter=72\n",
    );

    let first = match request(&mut harness, audio, &load(&sfz)) {
        LoadInstrumentResult::Ok { instrument_id, name, resident_bytes } => {
            assert_eq!(name, "load");
            assert!(resident_bytes > 0, "resident bytes not reported");
            instrument_id
        }
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    };
    match request(&mut harness, audio, &load(&sfz)) {
        LoadInstrumentResult::Ok { instrument_id, .. } => {
            assert!(instrument_id > first, "a second load must take a later id: {first} then {instrument_id}");
        }
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
    }
}

/// Two bank loads in flight at once whose `.sfz` files name the same sample.
/// Catches sample reads demuxed by path rather than by request context: each
/// load must fill its own slot and answer its own caller.
#[test]
fn concurrent_bank_loads_of_one_sample_answer_their_own_callers() {
    let (mut harness, audio) = boot(AudioOutput::Null);
    write_fixture("shared-bank.wav", &wav(256));
    let region = b"<region>\nsample=shared-bank.wav pitch_keycenter=60\n";
    let bank_a = write_fixture("bank_a.sfz", region);
    let bank_b = write_fixture("bank_b.sfz", region);

    let first = harness.send_deferred(audio, &load(&bank_a));
    let second = harness.send_deferred(audio, &load(&bank_b));

    for (pending, expected) in [(second, "bank_b"), (first, "bank_a")] {
        match harness.await_deferred::<LoadInstrumentResult>(pending).expect("the bank load replies") {
            LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, expected),
            LoadInstrumentResult::Err { error, .. } => panic!("expected Ok for {expected}: {error}"),
        }
    }
}

/// A track load and a bank load in flight at once, reading the same file.
/// Catches a `ReadResult` routed to the wrong kind of load: each reply must
/// reach its own caller with its own shape.
#[test]
fn interleaved_track_and_bank_loads_answer_their_own_callers() {
    let (mut harness, audio) = boot(AudioOutput::Null);
    let shared = write_fixture("shared-track.wav", &wav(256));
    let sfz = write_fixture("shared-track.sfz", b"<region>\nsample=shared-track.wav pitch_keycenter=60\n");

    let track = harness.send_deferred(audio, &play(&shared, Some("intro")));
    let bank = harness.send_deferred(audio, &load(&sfz));

    match harness.await_deferred::<LoadInstrumentResult>(bank).expect("the bank load replies") {
        LoadInstrumentResult::Ok { name, .. } => assert_eq!(name, "shared-track"),
        LoadInstrumentResult::Err { error, .. } => panic!("expected Ok for the bank: {error}"),
    }
    match harness.await_deferred::<PlayTrackResult>(track).expect("the track load replies") {
        PlayTrackResult::Ok { lane, .. } => assert_eq!(lane.as_deref(), Some("intro")),
        PlayTrackResult::Err { error, .. } => panic!("expected Ok for the track: {error}"),
    }
}

/// Catches `load_instrument`'s three failure arms — a missing `.sfz`, a
/// malformed `.sfz`, and a missing sample — answering `Ok` or never
/// answering, and losing the cause the caller needs.
#[test]
fn load_instrument_replies_err_for_each_failed_step() {
    let (mut harness, audio) = boot(AudioOutput::Null);

    match request(&mut harness, audio, &load("missing.sfz")) {
        LoadInstrumentResult::Err { path, error, .. } => {
            assert_eq!(path, "missing.sfz");
            assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
        }
        LoadInstrumentResult::Ok { .. } => panic!("expected Err for a missing sfz"),
    }

    let malformed = write_fixture("malformed.sfz", b"<control>\ndefault_path=x/\n");
    match request(&mut harness, audio, &load(&malformed)) {
        LoadInstrumentResult::Err { error, .. } => {
            assert!(error.contains("parse"), "parse error not surfaced: {error}");
        }
        LoadInstrumentResult::Ok { .. } => panic!("expected Err for a malformed sfz"),
    }

    let sampleless = write_fixture("sampleless.sfz", b"<region>\nsample=absent.wav\n");
    match request(&mut harness, audio, &load(&sampleless)) {
        LoadInstrumentResult::Err { error, .. } => {
            assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
        }
        LoadInstrumentResult::Ok { .. } => panic!("expected Err for a missing sample"),
    }
}
