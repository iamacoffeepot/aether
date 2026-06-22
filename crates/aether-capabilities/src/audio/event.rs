//! The audio event channel (ADR-0039 §threading). The lock-free queue
//! between the cap's handlers and the cpal callback consumer, plus the
//! `AudioEvent` payloads that cross it.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use aether_data::MailboxId;

use super::kinds::ScheduledEvent;
use super::sample::SampleBank;

/// Capacity of the event queue between the cap's handlers and the
/// audio-callback consumer. 1024 slots hold ~10 seconds of a dense
/// 100-note-per-second stream; overflow is warn-dropped, which the
/// ADR-0039 timing-quantization section already documents as a v1
/// limitation (tight-burst percussion may drop notes under load).
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

/// Event a handler pushes into the audio callback's queue. The
/// `sender_mailbox` is baked in here (not re-derived on the callback
/// side) so the callback stays branch-minimal.
///
/// Not `Copy`: the track-start variant carries an `Arc`'d PCM buffer
/// (the decoded asset) and owned namespace / path strings (ADR-0103
/// §3). The queue never required `Copy`.
#[derive(Clone, Debug)]
pub enum AudioEvent {
    NoteOn {
        sender_mailbox: MailboxId,
        pitch: u8,
        velocity: u8,
        instrument_id: u8,
    },
    NoteOff {
        sender_mailbox: MailboxId,
        pitch: u8,
        instrument_id: u8,
    },
    SetMasterGain {
        gain: f32,
    },
    /// Start (or restart) a track in the dedicated mixer lane. `pcm`
    /// is already mono and resampled to the device rate, so the
    /// callback walks it by index. Keyed by `(sender_mailbox, lane,
    /// namespace, path)` — re-sending the same key restarts the track.
    TrackStart {
        sender_mailbox: MailboxId,
        lane: Option<String>,
        namespace: String,
        path: String,
        pcm: Arc<[f32]>,
        gain: f32,
        looping: bool,
    },
    /// Fade out and retire the track at this key. A no-op if no track
    /// matches (matching `note_off`).
    TrackStop {
        sender_mailbox: MailboxId,
        lane: Option<String>,
        namespace: String,
        path: String,
    },
    /// Append a loaded sampled-instrument bank to the synth's registry
    /// (ADR-0103 §4). The cap assigns `id` from `BUILTINS.len()` upward
    /// in load order and the synth pushes the bank in receipt order, so
    /// the two stay in lockstep — a `note_on` whose `instrument_id`
    /// walks past the built-ins indexes this table. `bank` is the
    /// assembled, device-rate PCM bank behind an `Arc` (shared with the
    /// voices it spawns).
    RegisterInstrument {
        id: u8,
        bank: Arc<SampleBank>,
    },
    /// A validated batch of timed note events (ADR-0104). `sender_mailbox`
    /// is the scheduling sender, baked in so every scheduled note keys
    /// its voice (and note-off matching) by the original caller. The
    /// synth converts each event's `at_millis` to an absolute due frame
    /// against its frame clock at the instant it drains this event, so
    /// the whole batch shares one receipt timebase and chords stay
    /// aligned. One queue slot carries the entire tune.
    Schedule {
        sender_mailbox: MailboxId,
        events: Vec<ScheduledEvent>,
    },
}

/// Producer side of the audio event queue. The cap holds one (after
/// building the pipeline) and pushes events on every inbound `NoteOn`
/// / `NoteOff` / `SetMasterGain`.
#[derive(Clone)]
pub struct AudioEventSender {
    pub queue: Arc<ArrayQueue<AudioEvent>>,
}

impl AudioEventSender {
    pub fn push(&self, event: AudioEvent) -> Result<(), AudioEvent> {
        self.queue.push(event)
    }
}

pub fn new_event_channel() -> (AudioEventSender, Arc<ArrayQueue<AudioEvent>>) {
    let queue = Arc::new(ArrayQueue::new(EVENT_QUEUE_CAPACITY));
    (
        AudioEventSender {
            queue: Arc::clone(&queue),
        },
        queue,
    )
}
