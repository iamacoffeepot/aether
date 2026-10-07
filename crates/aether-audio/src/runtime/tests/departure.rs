use super::super::event::TrackStart;
use super::*;
use aether_actor::ErasedActorRef;

fn sender(name: &str, registry: &Registry) -> ErasedActorRef {
    registered_ref(registry, name, noop_handler())
}

fn departed_track(sender: ErasedActorRef) -> AudioEvent {
    AudioEvent::TrackStart(TrackStart {
        sender: Some(sender),
        lane: None,
        namespace: "assets".to_owned(),
        path: "track.wav".to_owned(),
        pcm: Arc::from(vec![0.25f32; 4_800]),
        gain: 1.0,
        looping: true,
    })
}

#[test]
fn departed_sender_track_fades_while_other_sender_plays_on() {
    let registry = Registry::new();
    let departed = sender("test.audio.departed.track", &registry);
    let survivor = sender("test.audio.survivor.track", &registry);

    let (queue, events) = new_event_channel();
    let mut synth = Synth::new(events, TEST_RATE);
    queue.push(departed_track(departed)).unwrap();
    queue.push(departed_track(survivor)).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.track_count(), 2, "setup: both senders track");

    queue.push(AudioEvent::SenderDeparted { sender: departed }).unwrap();
    let mut tail = vec![0.0f32; 512];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.track_count(), 1, "the departed sender track must fade and retire");

    queue.push(AudioEvent::SenderDeparted { sender: survivor }).unwrap();
    synth.fill(&mut tail, 1);
    assert_eq!(synth.track_count(), 0, "the survivor track retires only on its own departure");
}

#[test]
fn departed_sender_voice_releases_while_other_sender_sustains() {
    let registry = Registry::new();
    let departed = sender("test.audio.departed.voice", &registry);
    let survivor = sender("test.audio.survivor.voice", &registry);

    let (queue, events) = new_event_channel();
    let mut synth = Synth::new(events, TEST_RATE);
    queue
        .push(AudioEvent::NoteOn { sender: Some(departed), pitch: 60, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    queue
        .push(AudioEvent::NoteOn { sender: Some(survivor), pitch: 72, velocity: 100, instrument_id: 0, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 480];
    synth.fill(&mut buf, 1);
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 2, "setup: both senders voice");

    queue.push(AudioEvent::SenderDeparted { sender: departed }).unwrap();
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 2, "a departed voice must release through its envelope, not vanish");
    assert!(synth.has_voice_with_pitch(60), "the releasing voice must still be present");
    assert!(synth.has_voice_with_pitch(72), "the survivor must keep sounding");

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let release_samples = (0.6 * TEST_RATE) as usize;
    let mut tail = vec![0.0f32; release_samples];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.voice_count(), 1, "the departed voice must retire, leaving the survivor");
    assert!(synth.has_voice_with_pitch(72), "the survivor must keep sounding");
    assert!(!synth.has_voice_with_pitch(60), "the departed voice must be silent");
}

#[test]
fn departed_sender_scheduled_batch_never_fires() {
    let registry = Registry::new();
    let departed = sender("test.audio.departed.schedule", &registry);
    let survivor = sender("test.audio.survivor.schedule", &registry);

    let (queue, events) = new_event_channel();
    let mut synth = Synth::new(events, TEST_RATE);
    queue
        .push(AudioEvent::Schedule {
            sender: Some(departed),
            events: vec![ScheduledEvent {
                at_millis: 600_000,
                event: ScheduledNote::On { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 },
            }],
        })
        .unwrap();
    queue
        .push(AudioEvent::Schedule {
            sender: Some(survivor),
            events: vec![ScheduledEvent {
                at_millis: 0,
                event: ScheduledNote::On { pitch: 72, velocity: 100, instrument_id: 0, pan: 0 },
            }],
        })
        .unwrap();
    queue.push(AudioEvent::SenderDeparted { sender: departed }).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.voice_count(), 1, "only the survivor scheduled note must fire");
    assert!(synth.has_voice_with_pitch(72), "the survivor pitch must sound");
    assert!(!synth.has_voice_with_pitch(60), "the departed far-horizon note must never spawn a voice");
    assert_eq!(synth.scheduled_count(), 0, "the departed heap entry must be dropped");
}

#[test]
fn departed_sender_gain_holds_through_tail_then_prunes() {
    let registry = Registry::new();
    let departed = sender("test.audio.departed.gain", &registry);
    let live = sender("test.audio.live.gain", &registry);

    let (queue, events) = new_event_channel();
    let mut synth = Synth::new(events, TEST_RATE);
    queue.push(AudioEvent::SetSenderGain { sender: Some(departed), gain: 0.25 }).unwrap();
    queue
        .push(AudioEvent::NoteOn { sender: Some(departed), pitch: 60, velocity: 80, instrument_id: 0, pan: 0 })
        .unwrap();
    let mut buf = vec![0.0f32; 480];
    synth.fill(&mut buf, 1);
    synth.fill(&mut buf, 1);
    let steady: f32 = buf.iter().map(|sample| sample.abs()).sum();

    queue.push(AudioEvent::SenderDeparted { sender: departed }).unwrap();
    synth.fill(&mut buf, 1);
    let tail: f32 = buf.iter().map(|sample| sample.abs()).sum();
    assert!(
        tail < steady * 2.0,
        "the release tail must hold its trim, not jump to unity: steady={steady}, tail={tail}",
    );
    assert!(tail > 0.0, "the release tail must still sound");

    let (queue, events) = new_event_channel();
    let mut synth = Synth::new(events, TEST_RATE);
    queue.push(AudioEvent::SetSenderGain { sender: Some(departed), gain: 0.25 }).unwrap();
    queue.push(AudioEvent::SetSenderGain { sender: Some(live), gain: 0.5 }).unwrap();
    queue
        .push(AudioEvent::NoteOn { sender: Some(departed), pitch: 60, velocity: 80, instrument_id: 0, pan: 0 })
        .unwrap();
    queue.push(departed_track(departed)).unwrap();
    let mut buf = vec![0.0f32; 64];
    synth.fill(&mut buf, 1);
    assert_eq!(synth.sender_gain_count(), 2, "setup: both senders hold a gain row");

    queue.push(AudioEvent::SenderDeparted { sender: departed }).unwrap();
    let mut tail = vec![0.0f32; 512];
    synth.fill(&mut tail, 1);
    assert_eq!(synth.track_count(), 0, "the departed track must fade and retire");
    assert_eq!(synth.voice_count(), 1, "the departed voice must still release");
    assert_eq!(synth.sender_gain_count(), 2, "the row must live while the voice still references it");

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let release_samples = (0.6 * TEST_RATE) as usize;
    let mut rest = vec![0.0f32; release_samples];
    synth.fill(&mut rest, 1);
    assert_eq!(synth.voice_count(), 0, "the departed voice must retire");
    assert_eq!(synth.sender_gain_count(), 1, "the departed row must prune once nothing references it");
}
