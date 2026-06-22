//! The `aether.audio.*` mail vocabulary (ADR-0121: the capability owns
//! its kinds). The 11 audio kinds — the cast-shaped real-time triggers
//! (`NoteOn` / `NoteOff` / `SetMasterGain`) and the structured control
//! plane (`SetMasterGain`'s reply, the ADR-0104 scheduled batch, the
//! ADR-0103 track lane, and the ADR-0103 sampled-bank loader) — live
//! here under the always-on `audio` marker, so a wasm guest addressing
//! the cap through the marker feature sees the types without the
//! `audio-native` synth stack. Re-exported at the `audio` module root
//! (`pub use kinds::*`), so `aether_capabilities::audio::NoteOn`
//! resolves for callers.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Start a note playing on the desktop chassis's MIDI synth (ADR-0039).
/// `pitch` is a standard MIDI note number (0–127, middle C = 60).
/// `velocity` is 0–127 (MIDI convention; 0 has the same effect as a
/// `NoteOff`, but agents should prefer `NoteOff` for clarity).
/// `instrument_id` indexes the substrate-resident instrument registry
/// — v1 ships a fixed set; future patch-based instruments (Phase 2
/// follow-up) will extend the id space without a wire change. The
/// substrate keys the allocated voice by `(sender_mailbox, instrument_id,
/// pitch)` so same-pitch notes from different senders or different
/// instruments don't stomp each other. Fire-and-forget; no reply.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.audio.note_on")]
pub struct NoteOn {
    pub pitch: u8,
    pub velocity: u8,
    pub instrument_id: u8,
}

/// Release a note previously started with `NoteOn`. The substrate
/// matches on `(sender_mailbox, instrument_id, pitch)` — the sender
/// is taken from the mail envelope, not carried in the payload. A
/// `NoteOff` that doesn't match any live voice is silently ignored
/// (normal during race windows between envelope release and late
/// note-offs). Fire-and-forget; no reply.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Pod,
    Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.audio.note_off")]
pub struct NoteOff {
    pub pitch: u8,
    pub instrument_id: u8,
}

/// Set the substrate's master audio gain. `gain` is a linear scalar
/// applied to the summed voice output before the cpal device buffer;
/// `1.0` is unity, `0.0` mutes, values above `1.0` are clamped to
/// avoid clipping. This is the only substrate-level gain control —
/// per-source and bus-level attenuation are user-space concerns (ADR-0039).
/// Desktop-only: headless and hub chassis reply with an
/// `unsupported on <chassis>` error. Fire-and-forget in the happy path.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.audio.set_master_gain")]
pub struct SetMasterGain {
    pub gain: f32,
}

/// Reply to `SetMasterGain` (ADR-0039). `Ok` echoes the gain the
/// substrate actually applied — values above `1.0` are clamped, so
/// callers that sent `1.5` learn they got `1.0`. `Err` fires on
/// chassis without an audio device (headless, hub) or when audio
/// was disabled at boot via `AETHER_AUDIO_DISABLE`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.set_master_gain_result")]
pub enum SetMasterGainResult {
    Ok { applied_gain: f32 },
    Err { error: String },
}

// ADR-0104 scheduled note events. One `aether.audio.schedule` mail
// carries a whole tune as a batch of timed note events; the audio cap
// schedules them against its own sample clock so relative timing is
// sample-accurate. Structured-shaped — the batch is a `Vec`, not a
// cast-eligible `#[repr(C)]` body.

/// One note action in a scheduled batch (ADR-0104). The payload
/// mirrors `note_on` / `note_off` exactly — a scheduled note allocates
/// from the same voice pool, obeys the same steal policy, and keys
/// note-off matching by the scheduling sender, as if the equivalent
/// mail had arrived at the event's due instant.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ScheduledNote {
    On {
        pitch: u8,
        velocity: u8,
        instrument_id: u8,
    },
    Off {
        pitch: u8,
        instrument_id: u8,
    },
}

/// A timed entry in an `aether.audio.schedule` batch (ADR-0104).
/// `at_millis` is the play-at offset relative to the batch's arrival
/// at the audio callback, so every event in one batch shares a single
/// timebase and simultaneous events (a chord) stay aligned. Offsets
/// run forward from receipt; there is no notion of a past due time.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEvent {
    pub at_millis: u32,
    pub event: ScheduledNote,
}

/// `aether.audio.schedule` — dispatch a batch of timed note events in
/// a single mail (ADR-0104), so a melody plays with correct relative
/// timing instead of collapsing into a cluster chord. The cap
/// validates the batch synchronously — an events-per-batch cap and a
/// horizon cap on `at_millis`, rejecting the whole batch atomically on
/// any invalid entry — and replies `ScheduleResult` in-handler. The
/// accepted batch crosses to the audio callback as one event; the
/// synth converts each `at_millis` to an absolute due frame at receipt
/// and fires the events sample-accurately inside its render loop.
/// Desktop-only — chassis without an audio device reply `Err`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.schedule")]
pub struct Schedule {
    pub events: Vec<ScheduledEvent>,
}

/// Reply to `Schedule`. `Ok { accepted }` reports how many events the
/// batch admitted — a score player can trust that an `Ok` batch plays
/// in full, since validation is atomic. `Err` carries a human-readable
/// reason — an over-cap batch size, an over-horizon `at_millis`, or a
/// chassis without an audio device — loud rather than logged-and-
/// dropped (ADR-0104).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.schedule_result")]
pub enum ScheduleResult {
    Ok { accepted: u32 },
    Err { error: String },
}

// ADR-0103 track playback. The audio cap plays a decoded audio asset
// (music, ambience) in its own mixer lane, addressed by fs namespace
// + path the way the rest of the substrate addresses files (ADR-0041).
// Structured-shaped because every field is a `String` / `f32` / `bool`,
// not a cast-eligible `#[repr(C)]` body like `NoteOn`.

/// `aether.audio.play_track` — fetch, decode, and play an audio asset
/// through the audio cap. The cap forwards an `aether.fs.read` for
/// `namespace://path`, decodes + resamples the bytes off the realtime
/// path, and mixes the track in its own lane — never counted against
/// the voice pool, never voice-stolen. `gain` is a linear per-track
/// scalar applied at play time; `looping` wraps the track to its start
/// on completion instead of retiring it. Re-playing the same
/// `(sender, lane, namespace, path)` key restarts the track. Reply:
/// `PlayTrackResult`. Desktop-only — chassis without an audio device
/// reply `Err` (ADR-0103 §7).
///
/// `lane` augments the track key so callers that share a source
/// mailbox can each own a distinct track under the same
/// `(namespace, path)`. Senders are distinguished by their envelope
/// mailbox, but non-component senders — MCP sessions, substrate-
/// internal mail — all collapse to one mailbox id, so two such callers
/// would otherwise alias to a single track and stop or restart each
/// other. Each passes its own `lane` string to stay isolated; `None`
/// is exactly the unlaned behavior. Isolation is cooperative, not
/// enforced — a sender that names another's `(sender, lane)` collides
/// deliberately, which is the right strength inside one trust domain.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.play_track")]
pub struct PlayTrack {
    pub namespace: String,
    pub path: String,
    pub gain: f32,
    pub looping: bool,
    pub lane: Option<String>,
}

/// Reply to `PlayTrack`. Both arms echo the originating `lane` +
/// `namespace` + `path` for correlation — a caller running several
/// lanes over the same path tells the replies apart by `lane`. `Ok`
/// fires once the asset has decoded and the track started in the mixer
/// lane; `Err` carries a human-readable reason — a typo'd path (the fs
/// error), a malformed / unsupported file (the decode error), or a
/// chassis without an audio device. A bad path comes back loud rather
/// than logged-and-dropped because it is the common agent failure
/// (ADR-0103 §2).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.play_track_result")]
pub enum PlayTrackResult {
    Ok {
        namespace: String,
        path: String,
        lane: Option<String>,
    },
    Err {
        namespace: String,
        path: String,
        lane: Option<String>,
        error: String,
    },
}

/// `aether.audio.stop_track` — fade out and retire a track started by
/// `PlayTrack`. Matched on `(sender, lane, namespace, path)` — the
/// sender is taken from the mail envelope, not the payload — so one
/// component cannot stop another's track. `lane` must match the value
/// the `PlayTrack` carried (an unlaned track stops with `None`); it
/// lets callers that share a source mailbox stop only their own lane.
/// Releases through a short (~5 millisecond) linear fade to avoid a
/// click. Stopping a track that isn't playing is a no-op, matching
/// `note_off`. Fire-and-forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.stop_track")]
pub struct StopTrack {
    pub namespace: String,
    pub path: String,
    pub lane: Option<String>,
}

// ADR-0103 sampled instrument banks. The audio cap loads a bank of
// pitched samples at runtime, appends it to the instrument registry
// past the compiled-in built-ins, and plays it through the unchanged
// `note_on` / `note_off` surface (a third voice kernel beside the
// oscillator and partial-bank patches). Structured-shaped — the request
// carries `String` namespace/path, the reply a numeric id + name.

/// `aether.audio.load_instrument` — load a sampled instrument bank
/// from an `.sfz` file in an fs namespace. The cap fetches the `.sfz`
/// through `aether.fs`, parses the SFZ subset (regions, key / velocity
/// ranges, root pitch), fetches every WAV it references, decodes and
/// resamples them off the realtime path, assembles the bank, and
/// appends it to the registry at the next id past the built-ins. The
/// assigned id rides the reply; a subsequent `note_on` with that id
/// plays the sampled instrument. Loaded ids are session-scoped — they
/// depend on load order and do not survive a restart (ADR-0103 §4).
/// Reply: `LoadInstrumentResult`. Desktop-only — chassis without an
/// audio device reply `Err` (ADR-0103 §7).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.load_instrument")]
pub struct LoadInstrument {
    pub namespace: String,
    pub path: String,
}

/// Reply to `LoadInstrument`. `Ok` carries the `instrument_id` the
/// bank was assigned (thread it into `NoteOn.instrument_id` to play
/// it), the `name` derived from the `.sfz` filename, and
/// `resident_bytes` — the decoded PCM the bank holds resident, so an
/// agent can see what a load is spending (no bank unload in v1, ADR-0103
/// §4). `Err` echoes the originating `namespace` + `path` with a
/// human-readable reason — a typo'd path (the fs error), a malformed
/// `.sfz` or sample (the parse / decode error), or a chassis without
/// an audio device — loud rather than logged-and-dropped (ADR-0103 §2).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.audio.load_instrument_result")]
pub enum LoadInstrumentResult {
    Ok {
        instrument_id: u8,
        name: String,
        resident_bytes: u64,
    },
    Err {
        namespace: String,
        path: String,
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;
    use aether_kinds::descriptors;

    #[test]
    fn audio_kind_names() {
        assert_eq!(NoteOn::NAME, "aether.audio.note_on");
        assert_eq!(NoteOff::NAME, "aether.audio.note_off");
        assert_eq!(SetMasterGain::NAME, "aether.audio.set_master_gain");
        assert_eq!(
            SetMasterGainResult::NAME,
            "aether.audio.set_master_gain_result"
        );
        assert_eq!(Schedule::NAME, "aether.audio.schedule");
        assert_eq!(ScheduleResult::NAME, "aether.audio.schedule_result");
        assert_eq!(PlayTrack::NAME, "aether.audio.play_track");
        assert_eq!(PlayTrackResult::NAME, "aether.audio.play_track_result");
        assert_eq!(StopTrack::NAME, "aether.audio.stop_track");
        assert_eq!(LoadInstrument::NAME, "aether.audio.load_instrument");
        assert_eq!(
            LoadInstrumentResult::NAME,
            "aether.audio.load_instrument_result"
        );
    }

    #[test]
    fn schedule_batch_round_trip() {
        let schedule = Schedule {
            events: vec![
                ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On {
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                    },
                },
                ScheduledEvent {
                    at_millis: 250,
                    event: ScheduledNote::Off {
                        pitch: 60,
                        instrument_id: 0,
                    },
                },
            ],
        };
        let back = Schedule::decode_from_bytes(&schedule.encode_into_bytes())
            .expect("decode Schedule round-trip");
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.events[0].at_millis, 0);
        assert!(matches!(
            back.events[0].event,
            ScheduledNote::On {
                pitch: 60,
                velocity: 100,
                instrument_id: 0,
            }
        ));
        assert_eq!(back.events[1].at_millis, 250);
        assert!(matches!(
            back.events[1].event,
            ScheduledNote::Off {
                pitch: 60,
                instrument_id: 0,
            }
        ));
    }

    #[test]
    fn schedule_result_round_trip() {
        let ok = ScheduleResult::Ok { accepted: 7 };
        let back = ScheduleResult::decode_from_bytes(&ok.encode_into_bytes())
            .expect("decode ScheduleResult::Ok round-trip");
        assert!(matches!(back, ScheduleResult::Ok { accepted: 7 }));

        let err = ScheduleResult::Err {
            error: "batch exceeds the 8192-event cap".into(),
        };
        let back = ScheduleResult::decode_from_bytes(&err.encode_into_bytes())
            .expect("decode ScheduleResult::Err round-trip");
        match back {
            ScheduleResult::Err { error } => assert!(error.contains("8192-event")),
            ScheduleResult::Ok { .. } => panic!("expected Err"),
        }
    }

    /// The capability's `Kind` derives emit a native `inventory::submit!`
    /// for each kind, so the audio names appear in the global descriptor
    /// inventory the moment `aether-capabilities` is linked — which every
    /// chassis binary does. This proves `describe_kinds` / the descriptor
    /// inventory still surface all 11 audio kinds after the move out of
    /// `aether-kinds`.
    #[test]
    fn audio_kinds_in_descriptor_inventory() {
        let all = descriptors::all();
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        for name in [
            NoteOn::NAME,
            NoteOff::NAME,
            SetMasterGain::NAME,
            SetMasterGainResult::NAME,
            Schedule::NAME,
            ScheduleResult::NAME,
            PlayTrack::NAME,
            PlayTrackResult::NAME,
            StopTrack::NAME,
            LoadInstrument::NAME,
            LoadInstrumentResult::NAME,
        ] {
            assert!(
                names.contains(&name),
                "audio kind {name} missing from the descriptor inventory"
            );
        }
    }
}
