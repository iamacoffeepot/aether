use super::super::event::TrackStart;
use super::*;
use aether_actor::ErasedActorRef;

// ADR-0103 track lane. These drive `Synth` directly (the same pattern as
// the note tests); the cap's handlers are covered by `tests/handlers.rs`.

/// A short ramp track at the device rate — long enough to span a
/// few `fill` blocks but cheap to play to completion.
fn ramp_pcm(len: usize) -> Arc<[f32]> {
    // Index-to-float over a small range — exact in f32.
    #[allow(clippy::cast_precision_loss)]
    let v: Vec<f32> = (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect();
    Arc::from(v)
}

fn track_start(pcm: Arc<[f32]>, looping: bool) -> AudioEvent {
    AudioEvent::TrackStart(TrackStart {
        sender: None,
        lane: None,
        namespace: "assets".to_owned(),
        path: "track.wav".to_owned(),
        pcm,
        gain: 1.0,
        looping,
    })
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
            sender: None,
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
    for _ in 0..MAX_VOICES + 8 {
        sender.push(AudioEvent::NoteOn { sender: None, pitch: 60, velocity: 100, instrument_id: 0, pan: 0 }).unwrap();
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
fn keyed_track_start(sender: Option<ErasedActorRef>, lane: Option<&str>, pcm: Arc<[f32]>) -> AudioEvent {
    AudioEvent::TrackStart(TrackStart {
        sender,
        lane: lane.map(str::to_owned),
        namespace: "assets".to_owned(),
        path: "track.wav".to_owned(),
        pcm,
        gain: 1.0,
        looping: true,
    })
}

#[test]
fn distinct_lanes_under_one_sender_play_independently() {
    let (sender, queue) = new_event_channel();
    let mut synth = Synth::new(queue, TEST_RATE);
    // Two callers that share the `None` sender key (session mail)
    // play the same path under distinct lanes.
    sender.push(keyed_track_start(None, Some("a"), ramp_pcm(4_800))).unwrap();
    sender.push(keyed_track_start(None, Some("b"), ramp_pcm(4_800))).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 2, "distinct lanes must not alias to one track");
    // Stopping lane a leaves lane b sounding.
    sender
        .push(AudioEvent::TrackStop {
            sender: None,
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
        sender.push(keyed_track_start(None, Some("a"), ramp_pcm(256))).unwrap();
    }
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 1, "re-playing the same (sender, lane) key must restart, not stack");
}
