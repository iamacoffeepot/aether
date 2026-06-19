//! Issue 545 PR E1: collapsed `aether.audio` cap. Pre-PR-E1 the cap
//! lived split across `aether-kinds::audio::AudioCapability<B>`
//! (facade generic) and this file (concrete `CpalAudioBackend`). The
//! facade pattern (ADR-0075) is retired — caps are now regular
//! `#[actor]` blocks, same shape as wasm components.
//!
//! ADR-0039 Phase 2 stack lives here — `cpal` output stream,
//! hand-rolled synth, built-in instrument registry — plus the
//! [`AudioCapability`] itself.
//!
//! Synthesis is hand-rolled (no `SoundFont`, no DSP graph library):
//! each voice runs one of two kernels — a waveform oscillator (a
//! periodic wave or a seeded noise source, optionally pitch-swept)
//! through a linear ADSR, or a fixed bank of inharmonic sine partials
//! with per-partial exponential decay — summed flat and scaled by
//! master gain. 11 built-in instruments cover the oscillator shapes
//! (sine / square / triangle / saw + a pluck-flavoured sawtooth), a
//! partial-bank piano, electric piano, and slow-swell pad, and a
//! noise / pitch-sweep percussion set (kick / hat / snare).
//! Per-source / bus-level mixing is deliberately not here — ADR-0039
//! commits to composing that in user-space via mixer components.
//!
//! ## Threading: per-cap audio worker
//!
//! `cpal::Stream` is `!Send` on macOS — it must live on the thread
//! that constructed it. The chassis dispatcher thread requires the
//! cap struct to be `Send` so it can move into the spawn closure.
//! Putting `Stream` on the cap directly would make the whole cap
//! `!Send`.
//!
//! Resolution: the cap spawns its own audio worker thread at
//! construction. The worker builds the `cpal::Stream`, parks on a
//! shutdown channel, and drops the stream when the channel
//! disconnects. The cap itself holds an
//! `Arc<ArrayQueue<AudioEvent>>` (Send) that the cpal callback
//! reads from; `on_note_on` / `on_note_off` push to that queue
//! directly with no thread hop. This is the one cap with a worker
//! thread — every other cap is single-threaded by design; cpal's
//! `!Send` constraint forces this exception.
//!
//! Cap lifecycle: dropping the cap drops the shutdown sender, the
//! worker's `recv()` returns, the worker exits dropping
//! `cpal::Stream`. The chassis dispatcher's drop sequence
//! (cap shutdown → cap drop → worker thread) handles this
//! transparently.
//!
//! ## Boot error policy
//!
//! cpal init failure is **not** fatal. Audio is a peripheral, not
//! infrastructure — a CI machine without an audio device should
//! still boot. If cpal fails (no device, rate unsupported,
//! `AETHER_AUDIO_DISABLE=1`), the cap falls back to nop:
//! `NoteOn` / `NoteOff` are dropped silently and `SetMasterGain`
//! replies `Err` so agents fail fast instead of hanging.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{
    LoadInstrument, NoteOff, NoteOn, PlayTrack, ReadResult, Schedule, SetMasterGain, StopTrack,
};

// `AudioConfig` rides through file root for chassis-bin consumers
// that build it from env (`from_env`) and pass it to
// `with_actor::<AudioCapability>(cfg)`. Native-only re-export — wasm
// components opting into the marker-only `audio` feature don't need
// the config struct (sends are typed; config is the chassis's
// concern).
#[cfg(all(not(target_arch = "wasm32"), feature = "audio-native"))]
pub use native::{AudioConfig, AudioConfigLayer, AudioOverlay};

// ADR-0103 §1 decode/resample core (`crates/aether-capabilities/src/audio/decode.rs`).
// Native-only — it pulls `hound` and `std`; the marker-only `audio`
// build skips it. The track lane (`on_play_track`) and the future
// sampled-instrument loader (#1679) both consume `decode_wav_to_mono`.
#[cfg(all(not(target_arch = "wasm32"), feature = "audio-native"))]
mod decode;

// ADR-0103 §5 SFZ-subset parser for sampled instrument banks (#1679).
// Pure (`&str → BankSpec`), no I/O — the cap fetches the `.sfz` text and
// every referenced sample through `aether.fs`, then this module turns the
// text into structure. Native-only alongside `decode`.
#[cfg(all(not(target_arch = "wasm32"), feature = "audio-native"))]
mod sfz;

#[aether_actor::bridge(singleton, feature = "audio-native")]
mod native {
    use std::cmp::{Ordering, Reverse};
    use std::collections::BinaryHeap;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::f32::consts::TAU;
    use std::io::Cursor;
    use std::str::from_utf8;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use crossbeam_queue::ArrayQueue;

    use aether_actor::{Manual, OutboundReply, actor};
    use aether_data::{MailboxId, Source, SourceAddr};
    use aether_kinds::{
        LoadInstrument, LoadInstrumentResult, PlayTrack, PlayTrackResult, Read, ReadResult,
        Schedule, ScheduleResult, ScheduledEvent, ScheduledNote, SetMasterGainResult, StopTrack,
    };

    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
    use aether_substrate::chassis::error::BootError;

    use crate::fs::FsCapability;

    use super::decode::{DecodeError, decode_wav_to_mono};
    use super::sfz::{SfzLoop, SfzRegion, parse_sfz};
    use super::{NoteOff, NoteOn, SetMasterGain};

    /// Linear fade-out duration (seconds) applied when a track is stopped,
    /// so `stop_track` releases through a short ramp instead of truncating
    /// to a click (ADR-0103 §3).
    const TRACK_FADE_SECS: f32 = 0.005;
    // confique consumes `parse_flag` through `#[config(parse_env = …)]`;
    // IntelliJ-Rust doesn't trace macro-attr path args (Qodana FP), but
    // rustc + clippy do.
    #[allow(unused_imports)]
    use crate::config_env::parse_flag;
    use core::fmt;

    /// Capacity of the event queue between the cap's handlers and the
    /// audio-callback consumer. 1024 slots hold ~10 seconds of a dense
    /// 100-note-per-second stream; overflow is warn-dropped, which the
    /// ADR-0039 timing-quantization section already documents as a v1
    /// limitation (tight-burst percussion may drop notes under load).
    const EVENT_QUEUE_CAPACITY: usize = 1024;

    /// Maximum concurrent voices before voice-stealing kicks in. Chosen
    /// as "more than a string section fits in one component" — on
    /// saturation, voice-steal always evicts the oldest sounding note,
    /// never causing audio glitches.
    const MAX_VOICES: usize = 64;

    /// Maximum note events one `aether.audio.schedule` batch may carry
    /// (ADR-0104). A batch crosses the event queue as a single slot, so
    /// this bounds the synth's pending heap rather than the queue; 8192
    /// events is several minutes of a dense melody (note-on + note-off per
    /// note). An over-cap batch rejects atomically in the handler reply.
    const SCHEDULE_MAX_EVENTS: usize = 8192;

    /// Furthest future a scheduled event may be parked, in milliseconds
    /// (ADR-0104). The horizon bounds how much future a sender can hold in
    /// the pending heap; ten minutes is generous for a tune dispatched in
    /// one call. An over-horizon `at_millis` rejects the whole batch.
    const SCHEDULE_MAX_MILLIS: u32 = 600_000;

    /// Resolved configuration for the audio synth. Chassis mains read
    /// env vars (`AETHER_AUDIO_DISABLE`, `AETHER_AUDIO_SAMPLE_RATE`)
    /// into an `AudioConfig` and pass it to `with_actor::<AudioCapability>(cfg)`
    /// (issue 464). Tests build an `AudioConfig` directly.
    ///
    /// ADR-0090 unit g (iamacoffeepot/aether#1264): the
    /// `#[derive(aether_substrate::Config)]` emits the env-shaped
    /// `AudioConfigLayer`, the clap-shaped `AudioOverlay`, the
    /// `FromArgvThenEnv` impl, and the inherent `from_env` /
    /// `from_argv_then_env` shims. `requested_sample_rate`'s type
    /// `Option<u32>` triggers the macro's type-driven
    /// `Option<numeric>` shape: the Layer holds `Option<String>` and
    /// `from_layer` does the soft `.parse().ok()` so an unparseable
    /// value lands as `None` (indistinguishable from unset, matching
    /// the prior reader).
    #[derive(Clone, Debug, Default, aether_substrate::Config)]
    #[config(env_prefix = "AETHER_AUDIO", cli_prefix = "audio")]
    pub struct AudioConfig {
        /// `AETHER_AUDIO_DISABLE=1` skips cpal init entirely. The cap
        /// still claims its mailbox and replies `Err` to `SetMasterGain`
        /// so agents fail fast instead of hanging. `env` + `cli_long`
        /// overrides pin the historical wire shape (no `D` suffix on
        /// `DISABLE`; `--audio-disable` not `--audio-disabled`).
        #[config(
            env = "AETHER_AUDIO_DISABLE",
            cli_long = "audio-disable",
            default = false,
            parse = parse_flag
        )]
        pub disabled: bool,
        /// `AETHER_AUDIO_SAMPLE_RATE=<hz>` requests a specific rate. If
        /// the device doesn't support it, boot falls back to nop
        /// (ADR-0039 — non-fatal). `layer_field = "sample_rate"` drops
        /// the `requested_` prefix on the Layer / env / CLI side so the
        /// historical names are unchanged.
        #[config(layer_field = "sample_rate", env = "AETHER_AUDIO_SAMPLE_RATE")]
        pub requested_sample_rate: Option<u32>,
    }

    /// Event a handler pushes into the audio callback's queue. The
    /// `sender_mailbox` is baked in here (not re-derived on the callback
    /// side) so the callback stays branch-minimal.
    ///
    /// Not `Copy`: the track-start variant carries an `Arc`'d PCM buffer
    /// (the decoded asset) and owned namespace / path strings (ADR-0103
    /// §3). The queue never required `Copy`.
    #[derive(Clone, Debug)]
    enum AudioEvent {
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

    /// One pending scheduled note in the synth's min-heap (ADR-0104).
    /// Ordered by `(due_frame, seq)` only — `seq` is a monotonic stamp in
    /// batch-arrival order, so events that fall on the same frame fire in
    /// the order they were sent. The note payload takes no part in
    /// ordering, which is why the ordering impls are hand-written rather
    /// than derived (`ScheduledNote` is not `Ord`).
    struct ScheduledEntry {
        due_frame: u64,
        seq: u64,
        sender_mailbox: MailboxId,
        note: ScheduledNote,
    }

    impl PartialEq for ScheduledEntry {
        fn eq(&self, other: &Self) -> bool {
            (self.due_frame, self.seq) == (other.due_frame, other.seq)
        }
    }

    impl Eq for ScheduledEntry {}

    impl PartialOrd for ScheduledEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for ScheduledEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            (self.due_frame, self.seq).cmp(&(other.due_frame, other.seq))
        }
    }

    /// Convert a play-at offset in milliseconds to a frame count at the
    /// device rate (ADR-0104). Added to the frame clock at receipt to land
    /// the absolute due frame.
    fn millis_to_frames(at_millis: u32, sample_rate: f32) -> u64 {
        // `at_millis` is bounded by `SCHEDULE_MAX_MILLIS` and the device
        // rate is a small positive integer, so the product is well within
        // u64 and non-negative.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let frames = (f64::from(at_millis) / 1000.0 * f64::from(sample_rate)) as u64;
        frames
    }

    /// Producer side of the audio event queue. The cap holds one (after
    /// building the pipeline) and pushes events on every inbound `NoteOn`
    /// / `NoteOff` / `SetMasterGain`.
    #[derive(Clone)]
    struct AudioEventSender {
        queue: Arc<ArrayQueue<AudioEvent>>,
    }

    impl AudioEventSender {
        fn push(&self, event: AudioEvent) -> Result<(), AudioEvent> {
            self.queue.push(event)
        }
    }

    fn new_event_channel() -> (AudioEventSender, Arc<ArrayQueue<AudioEvent>>) {
        let queue = Arc::new(ArrayQueue::new(EVENT_QUEUE_CAPACITY));
        (
            AudioEventSender {
                queue: Arc::clone(&queue),
            },
            queue,
        )
    }

    /// Primitive waveform the oscillator shapes. `Saw` is a downward ramp
    /// scaled to ±1; `Pluck` reuses `Saw` geometry but pairs with a
    /// fast-decay envelope — kept implicit by the patch table.
    ///
    /// `Noise` is the percussion source: white noise from a per-voice
    /// xorshift32 PRNG (seeded from the voice key, so a fixed key renders
    /// the same sequence every run), shaped by a one-pole lowpass whose
    /// `lowpass` coefficient is a patch constant — `1.0` passes the raw
    /// white noise (bright, a hat), a smaller value smooths it (darker, a
    /// snare body). `tone_mix` blends in a fixed-level sine at the voice's
    /// base frequency under the noise (`0.0` is pure noise); it is the one
    /// patch field that turns a hat patch into a snare.
    #[derive(Copy, Clone, Debug)]
    enum Wave {
        Sine,
        Square,
        Triangle,
        Saw,
        Noise { lowpass: f32, tone_mix: f32 },
    }

    /// Envelope shape — linear segments at sample-rate resolution. Values
    /// are held in seconds; the voice converts to per-sample step on
    /// instantiation so the hot loop is add-only.
    #[derive(Copy, Clone, Debug)]
    struct Adsr {
        attack_s: f32,
        decay_s: f32,
        sustain: f32,
        release_s: f32,
    }

    /// Optional per-patch pitch envelope on the oscillator kernel — the
    /// whole identity of a kick. The voice's phase step is multiplied by
    /// a ratio that starts at `start_ratio` and decays exponentially
    /// toward `1.0` (the note's base frequency) with the given time
    /// constant. The decay is precomputed as a per-sample multiplier at
    /// `note_on`, so the hot loop pays one extra multiply.
    #[derive(Copy, Clone, Debug)]
    struct PitchSweep {
        /// Phase-step multiplier at the note's onset. `4.0` starts two
        /// octaves above the base frequency; `1.0` is no sweep.
        start_ratio: f32,
        /// Exponential time constant (seconds) of the fall back to the
        /// base frequency. Short (tens of millis) for a punchy kick.
        time_constant_secs: f32,
    }

    /// Number of sine partials in a partial-bank voice. Fixed so the
    /// voice stays `Copy` and stack-friendly in the pool; the hot loop is
    /// one `sin`, one multiply-accumulate, and one decay multiply per
    /// partial.
    const PARTIAL_COUNT: usize = 8;

    /// Reference pitch (MIDI C4) for partial-bank decay scaling. A note's
    /// per-partial decay rates scale by `f0 / REFERENCE_FREQ`, so higher
    /// notes ring shorter and lower notes longer.
    const REFERENCE_FREQ: f32 = 261.625_57;

    /// Relative amplitude below which a sustaining (un-released)
    /// partial-bank voice frees itself. Piano partials decay
    /// exponentially and never reach exactly zero, so the voice retires
    /// once its summed partial energy crosses this floor.
    const PARTIAL_SILENCE_FLOOR: f32 = 1.0e-4;

    /// A struck/sustained partial-bank voice patch. Partial `n` is tuned
    /// to `n * f0 * sqrt(1 + inharmonicity * n^2)` plus a small
    /// per-partial detune; `partial_amps` is the spectral shape (tilted
    /// toward upper partials by velocity via `brightness_tilt`); each
    /// partial decays at `decay_base * (1 + i * decay_spread)` scaled by
    /// pitch. A global attack/release ramp wraps the bank.
    #[derive(Copy, Clone, Debug)]
    struct PartialBankDef {
        /// Stiffness coefficient `B`: stretches overtone `n` to
        /// `n * f0 * sqrt(1 + B * n^2)`. `0.0` is perfectly harmonic.
        inharmonicity: f32,
        /// Per-partial base amplitude (the spectral shape). Normalised at
        /// `note_on` so overall level comes from velocity, not this sum.
        partial_amps: [f32; PARTIAL_COUNT],
        /// Fundamental decay rate (per second) at the reference pitch.
        /// `0.0` sustains indefinitely (the pad).
        decay_base: f32,
        /// Per-partial-index decay multiplier: partial `i` decays at
        /// `decay_base * (1 + i * decay_spread)`, so upper partials fade
        /// first (a string's brightness dropping as it rings).
        decay_spread: f32,
        /// Per-partial detune fraction. Alternating ± across partials
        /// gives the slow beating of a multi-string course.
        detune: f32,
        /// Velocity-to-brightness tilt. Higher velocity multiplies the
        /// upper partials' share so a harder strike reads brighter.
        brightness_tilt: f32,
        /// Global attack ramp (seconds). Near-zero for a struck string,
        /// long for the pad's slow swell.
        attack_s: f32,
        /// Global release ramp (seconds) on `note_off` — the damper.
        release_s: f32,
    }

    /// Voice kernel a patch selects. The five original patches stay
    /// `Oscillator`; the partial-bank patches add struck-string and
    /// sustained timbres without a wire change.
    #[derive(Copy, Clone, Debug)]
    enum VoiceDef {
        Oscillator { wave: Wave, adsr: Adsr },
        PartialBank(PartialBankDef),
    }

    /// Full instrument patch. Agents address instruments by numeric id
    /// into the built-in registry; the registry hands the voice a copy of
    /// this struct at `note_on` time so each voice is self-contained.
    #[derive(Copy, Clone, Debug)]
    struct InstrumentDef {
        name: &'static str,
        voice: VoiceDef,
        base_amp: f32,
        /// Optional pitch envelope. Applies only to the `Oscillator`
        /// kernel (the partial bank ignores it); `None` is the common
        /// case. `Some` is the falling-frequency thump of a kick.
        pitch_sweep: Option<PitchSweep>,
    }

    /// The v1 instrument registry. Index matches `NoteOn.instrument_id`.
    /// Reordering these is a breaking change on the wire — adds go at
    /// the end. Future follow-up: mailed patch definitions fill in past
    /// the built-ins (ADR-0039 "runtime-defined patches" parked item).
    const BUILTINS: &[InstrumentDef] = &[
        InstrumentDef {
            name: "sine_lead",
            voice: VoiceDef::Oscillator {
                wave: Wave::Sine,
                adsr: Adsr {
                    attack_s: 0.01,
                    decay_s: 0.08,
                    sustain: 0.7,
                    release_s: 0.18,
                },
            },
            base_amp: 0.35,
            pitch_sweep: None,
        },
        InstrumentDef {
            name: "square_bass",
            voice: VoiceDef::Oscillator {
                wave: Wave::Square,
                adsr: Adsr {
                    attack_s: 0.005,
                    decay_s: 0.12,
                    sustain: 0.6,
                    release_s: 0.12,
                },
            },
            base_amp: 0.22,
            pitch_sweep: None,
        },
        InstrumentDef {
            name: "triangle",
            voice: VoiceDef::Oscillator {
                wave: Wave::Triangle,
                adsr: Adsr {
                    attack_s: 0.02,
                    decay_s: 0.1,
                    sustain: 0.7,
                    release_s: 0.2,
                },
            },
            base_amp: 0.32,
            pitch_sweep: None,
        },
        InstrumentDef {
            name: "saw_lead",
            voice: VoiceDef::Oscillator {
                wave: Wave::Saw,
                adsr: Adsr {
                    attack_s: 0.01,
                    decay_s: 0.15,
                    sustain: 0.55,
                    release_s: 0.15,
                },
            },
            base_amp: 0.2,
            pitch_sweep: None,
        },
        InstrumentDef {
            name: "pluck",
            voice: VoiceDef::Oscillator {
                wave: Wave::Saw,
                adsr: Adsr {
                    attack_s: 0.002,
                    decay_s: 0.35,
                    sustain: 0.0,
                    release_s: 0.05,
                },
            },
            base_amp: 0.3,
            pitch_sweep: None,
        },
        // id 5: struck-string piano. Slightly stretched partials, a
        // bright-to-mellow decay (upper partials fade first), and a fast
        // damper release on note_off.
        InstrumentDef {
            name: "piano",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.000_4,
                partial_amps: [1.0, 0.6, 0.4, 0.25, 0.18, 0.12, 0.08, 0.05],
                decay_base: 3.0,
                decay_spread: 0.6,
                detune: 0.000_8,
                brightness_tilt: 0.5,
                attack_s: 0.002,
                release_s: 0.15,
            }),
            base_amp: 0.3,
            pitch_sweep: None,
        },
        // id 6: electric piano. Same partial-bank shape, more inharmonic
        // (bell-like), faster decay, and a brighter velocity response —
        // a pure patch-table entry, no new machinery.
        InstrumentDef {
            name: "electric_piano",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.001,
                partial_amps: [1.0, 0.3, 0.5, 0.2, 0.3, 0.15, 0.1, 0.06],
                decay_base: 4.0,
                decay_spread: 0.4,
                detune: 0.001_2,
                brightness_tilt: 0.7,
                attack_s: 0.003,
                release_s: 0.1,
            }),
            base_amp: 0.28,
            pitch_sweep: None,
        },
        // id 7: slow-swell pad. Harmonic partials, a long attack, near-
        // zero partial decay so it sustains while held, and a long
        // release — the warm sustained bed no oscillator patch can do.
        InstrumentDef {
            name: "pad",
            voice: VoiceDef::PartialBank(PartialBankDef {
                inharmonicity: 0.0,
                partial_amps: [1.0, 0.7, 0.5, 0.4, 0.3, 0.25, 0.2, 0.15],
                decay_base: 0.0,
                decay_spread: 0.0,
                detune: 0.000_6,
                brightness_tilt: 0.25,
                attack_s: 0.8,
                release_s: 0.6,
            }),
            base_amp: 0.18,
            pitch_sweep: None,
        },
        // id 8: kick. A sine swept down from two octaves above the base
        // frequency with a fast (30 ms) time constant, through a punchy
        // no-sustain ADSR — the falling thump that defines a kick. `pitch`
        // scales the base, so one patch covers kick through toms.
        InstrumentDef {
            name: "kick",
            voice: VoiceDef::Oscillator {
                wave: Wave::Sine,
                adsr: Adsr {
                    attack_s: 0.001,
                    decay_s: 0.18,
                    sustain: 0.0,
                    release_s: 0.02,
                },
            },
            base_amp: 0.9,
            pitch_sweep: Some(PitchSweep {
                start_ratio: 4.0,
                time_constant_secs: 0.03,
            }),
        },
        // id 9: hat. A short burst of bright (near-unfiltered) noise
        // through a fast no-sustain ADSR. `pitch` shifts the register so
        // one patch covers closed-versus-open flavours.
        InstrumentDef {
            name: "hat",
            voice: VoiceDef::Oscillator {
                wave: Wave::Noise {
                    lowpass: 0.9,
                    tone_mix: 0.0,
                },
                adsr: Adsr {
                    attack_s: 0.001,
                    decay_s: 0.04,
                    sustain: 0.0,
                    release_s: 0.02,
                },
            },
            base_amp: 0.4,
            pitch_sweep: None,
        },
        // id 10: snare. Darker (lowpassed) noise with a fixed-level sine
        // body mixed under it (`tone_mix`) — the one patch field that
        // separates a snare from a hat — through a short no-sustain ADSR.
        InstrumentDef {
            name: "snare",
            voice: VoiceDef::Oscillator {
                wave: Wave::Noise {
                    lowpass: 0.5,
                    tone_mix: 0.25,
                },
                adsr: Adsr {
                    attack_s: 0.001,
                    decay_s: 0.12,
                    sustain: 0.0,
                    release_s: 0.03,
                },
            },
            base_amp: 0.5,
            pitch_sweep: None,
        },
    ];

    fn instrument_by_id(id: u8) -> Option<&'static InstrumentDef> {
        BUILTINS.get(id as usize)
    }

    /// Number of built-in instruments. Used by the boot log so MCP
    /// agents can cross-reference.
    pub fn builtin_count() -> usize {
        BUILTINS.len()
    }

    /// Names of the built-in instruments, in id order.
    pub fn builtin_names() -> Vec<&'static str> {
        BUILTINS.iter().map(|d| d.name).collect()
    }

    /// Envelope state machine. `Release` captures the level it was at
    /// when the note was released, since a note can be released mid-attack
    /// or mid-decay — the release ramp starts from that value, not from
    /// the sustain level.
    #[derive(Copy, Clone, Debug)]
    enum EnvelopeStage {
        Attack { t: f32 },
        Decay { t: f32 },
        Sustain,
        Release { t: f32, from_level: f32 },
        Done,
    }

    /// The oscillator voice kernel — a periodic waveform through a linear
    /// ADSR. Every field is touched per sample, so the struct stays
    /// compact for cache friendliness in the voice pool.
    #[derive(Copy, Clone, Debug)]
    struct OscVoice {
        /// Oscillator phase in turns (`[0.0, 1.0)`), incremented by
        /// `freq / sample_rate` per sample.
        phase: f32,
        /// Turns-per-sample step — precomputed so the per-sample path is
        /// add-only.
        phase_step: f32,
        /// Base amplitude after velocity scaling; envelope multiplies this.
        amplitude: f32,
        wave: Wave,
        adsr: Adsr,
        envelope: EnvelopeStage,
        /// xorshift32 PRNG state for the `Noise` wave, seeded from the
        /// voice key. Unused (but harmless) for the periodic waves.
        rng: u32,
        /// One-pole lowpass memory for the `Noise` wave (the previous
        /// filtered output).
        lp_prev: f32,
        /// Current pitch-sweep offset added to `1.0` to scale `phase_step`
        /// this sample. `0.0` when the patch has no sweep.
        sweep_offset: f32,
        /// Per-sample multiplier the sweep offset decays by. `1.0` (no
        /// decay) when the patch has no sweep — the offset is then `0.0`,
        /// so the ratio stays `1.0`.
        sweep_decay: f32,
    }

    /// One step of an xorshift32 PRNG, mapped to white noise in `[-1.0,
    /// 1.0)`. The state is per-voice so percussion voices are independent
    /// and a fixed seed is reproducible.
    fn next_noise(state: &mut u32) -> f32 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *state = x;
        // Map the full u32 range to [-1.0, 1.0). The mantissa rounding is
        // inaudible and irrelevant to a noise source.
        #[allow(clippy::cast_precision_loss)]
        let frac = (x as f32) / (u32::MAX as f32);
        frac.mul_add(2.0, -1.0)
    }

    /// Seed the per-voice noise PRNG from the voice key
    /// (`sender_mailbox`, `instrument_id`, `pitch`) so a fixed key renders
    /// the same noise sequence every run. Forced non-zero — xorshift32 is
    /// stuck at zero.
    fn voice_seed(sender_mailbox: MailboxId, instrument_id: u8, pitch: u8) -> u32 {
        // Truncating the 64-bit mailbox id into the hash is intended; the
        // seed only needs to vary per key, not round-trip.
        #[allow(clippy::cast_possible_truncation)]
        let lo = sender_mailbox.0 as u32;
        #[allow(clippy::cast_possible_truncation)]
        let hi = (sender_mailbox.0 >> 32) as u32;
        let mixed = lo.wrapping_mul(2_654_435_761)
            ^ hi.wrapping_mul(40_503)
            ^ u32::from(instrument_id).wrapping_mul(2_246_822_519)
            ^ u32::from(pitch).wrapping_mul(3_266_489_917);
        mixed | 1
    }

    impl OscVoice {
        fn new(
            pitch: u8,
            velocity: u8,
            wave: Wave,
            adsr: Adsr,
            base_amp: f32,
            sample_rate: f32,
            seed: u32,
        ) -> Self {
            let freq = 440.0 * ((f32::from(pitch) - 69.0) / 12.0).exp2();
            let phase_step = freq / sample_rate;
            let v = f32::from(velocity) / 127.0;
            let amplitude = base_amp * v * v;
            Self {
                phase: 0.0,
                phase_step,
                amplitude,
                wave,
                adsr,
                envelope: EnvelopeStage::Attack { t: 0.0 },
                rng: seed,
                lp_prev: 0.0,
                sweep_offset: 0.0,
                sweep_decay: 1.0,
            }
        }

        /// Arm a pitch sweep on a freshly built voice. The offset starts
        /// at `start_ratio - 1.0` and decays by `sweep_decay` per sample;
        /// `next_sample` reads `1.0 + sweep_offset` as the phase-step
        /// multiplier. A non-positive time constant is treated as no
        /// sweep (the voice keeps its base frequency).
        fn with_pitch_sweep(mut self, sweep: PitchSweep, sample_rate: f32) -> Self {
            if sweep.time_constant_secs > 0.0 {
                let dt = 1.0 / sample_rate;
                self.sweep_offset = sweep.start_ratio - 1.0;
                self.sweep_decay = (-dt / sweep.time_constant_secs).exp();
            }
            self
        }

        fn note_off(&mut self) {
            let from_level = match self.envelope {
                EnvelopeStage::Attack { t } => {
                    if self.adsr.attack_s > 0.0 {
                        (t / self.adsr.attack_s).clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                }
                EnvelopeStage::Decay { t } => {
                    if self.adsr.decay_s > 0.0 {
                        let fall = (1.0 - self.adsr.sustain).mul_add(-(t / self.adsr.decay_s), 1.0);
                        fall.clamp(self.adsr.sustain.min(1.0), 1.0)
                    } else {
                        self.adsr.sustain
                    }
                }
                EnvelopeStage::Sustain => self.adsr.sustain,
                EnvelopeStage::Release { .. } | EnvelopeStage::Done => return,
            };
            self.envelope = EnvelopeStage::Release { t: 0.0, from_level };
        }

        fn done(&self) -> bool {
            matches!(self.envelope, EnvelopeStage::Done)
        }

        fn advance_envelope(&mut self, dt: f32) -> f32 {
            match &mut self.envelope {
                EnvelopeStage::Attack { t } => {
                    *t += dt;
                    if self.adsr.attack_s <= 0.0 || *t >= self.adsr.attack_s {
                        self.envelope = EnvelopeStage::Decay { t: 0.0 };
                        1.0
                    } else {
                        *t / self.adsr.attack_s
                    }
                }
                EnvelopeStage::Decay { t } => {
                    *t += dt;
                    if self.adsr.decay_s <= 0.0 || *t >= self.adsr.decay_s {
                        self.envelope = EnvelopeStage::Sustain;
                        self.adsr.sustain
                    } else {
                        (1.0 - self.adsr.sustain).mul_add(-(*t / self.adsr.decay_s), 1.0)
                    }
                }
                EnvelopeStage::Sustain => self.adsr.sustain,
                EnvelopeStage::Release { t, from_level } => {
                    *t += dt;
                    if self.adsr.release_s <= 0.0 || *t >= self.adsr.release_s {
                        self.envelope = EnvelopeStage::Done;
                        0.0
                    } else {
                        *from_level * (1.0 - (*t / self.adsr.release_s))
                    }
                }
                EnvelopeStage::Done => 0.0,
            }
        }

        /// Render the raw waveform at the current phase. Takes `&mut self`
        /// because the `Noise` wave advances its PRNG and one-pole filter
        /// state; the periodic waves only read `phase`.
        fn waveform(&mut self) -> f32 {
            match self.wave {
                Wave::Sine => (self.phase * TAU).sin(),
                Wave::Square => {
                    if self.phase < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                Wave::Triangle => {
                    if self.phase < 0.5 {
                        4.0f32.mul_add(self.phase, -1.0)
                    } else {
                        4.0f32.mul_add(-self.phase, 3.0)
                    }
                }
                Wave::Saw => 2.0f32.mul_add(-self.phase, 1.0),
                Wave::Noise { lowpass, tone_mix } => {
                    let white = next_noise(&mut self.rng);
                    // One-pole lowpass: y += coeff * (x - y). coeff 1.0
                    // passes the raw noise; smaller smooths it.
                    self.lp_prev = lowpass.mul_add(white - self.lp_prev, self.lp_prev);
                    let noise = self.lp_prev;
                    if tone_mix > 0.0 {
                        let tone = (self.phase * TAU).sin();
                        tone_mix.mul_add(tone, (1.0 - tone_mix) * noise)
                    } else {
                        noise
                    }
                }
            }
        }

        fn next_sample(&mut self, dt: f32) -> f32 {
            let env = self.advance_envelope(dt);
            let s = self.waveform() * self.amplitude * env;
            let ratio = 1.0 + self.sweep_offset;
            self.phase = self.phase_step.mul_add(ratio, self.phase);
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            self.sweep_offset *= self.sweep_decay;
            s
        }
    }

    /// One sine partial of a partial-bank voice. `amp` decays toward zero
    /// each sample by `decay_mul = exp(-rate * dt)`, so the hot loop holds
    /// no transcendentals beyond the `sin`.
    #[derive(Copy, Clone, Debug)]
    struct Partial {
        phase: f32,
        phase_step: f32,
        amp: f32,
        decay_mul: f32,
    }

    impl Partial {
        const SILENT: Self = Self {
            phase: 0.0,
            phase_step: 0.0,
            amp: 0.0,
            decay_mul: 1.0,
        };
    }

    /// Global attack/release ramp wrapping a partial bank. The partials
    /// carry their own per-sample decay; this ramp swells the voice in at
    /// `note_on` and damps it out at `note_off`.
    #[derive(Copy, Clone, Debug)]
    enum BankStage {
        Attack { t: f32 },
        Sustain,
        Release { t: f32, from_level: f32 },
        Done,
    }

    /// The partial-bank voice kernel — a fixed array of inharmonic sine
    /// partials with per-partial exponential decay, wrapped by a global
    /// attack/release ramp. Built once at `note_on`; the per-sample path
    /// is `sin` + multiply-accumulate + decay multiply per partial.
    #[derive(Copy, Clone, Debug)]
    struct PartialBankVoice {
        partials: [Partial; PARTIAL_COUNT],
        /// Overall level after velocity scaling; the partial amps carry
        /// the (normalised) spectral shape, this carries the loudness.
        amplitude: f32,
        stage: BankStage,
        attack_s: f32,
        release_s: f32,
    }

    impl PartialBankVoice {
        fn new(
            pitch: u8,
            velocity: u8,
            def: &PartialBankDef,
            base_amp: f32,
            sample_rate: f32,
        ) -> Self {
            let f0 = 440.0 * ((f32::from(pitch) - 69.0) / 12.0).exp2();
            let v = f32::from(velocity) / 127.0;
            let amplitude = base_amp * v;
            let pitch_scale = f0 / REFERENCE_FREQ;
            let dt = 1.0 / sample_rate;

            let mut partials = [Partial::SILENT; PARTIAL_COUNT];
            let mut total = 0.0f32;
            for (i, p) in partials.iter_mut().enumerate() {
                // PARTIAL_COUNT is 8 — the index-to-float casts are exact.
                #[allow(clippy::cast_precision_loss)]
                let i_f = i as f32;
                let n = i_f + 1.0;
                let stretch = (def.inharmonicity * n).mul_add(n, 1.0).sqrt();
                let detune = if i % 2 == 0 {
                    1.0 + def.detune
                } else {
                    1.0 - def.detune
                };
                p.phase_step = (n * f0 * stretch * detune) / sample_rate;
                let rate = def.decay_base * i_f.mul_add(def.decay_spread, 1.0) * pitch_scale;
                p.decay_mul = (-rate * dt).exp();
                let amp = def.partial_amps[i] * i_f.mul_add(def.brightness_tilt * v, 1.0);
                p.amp = amp;
                total += amp;
            }
            if total > 0.0 {
                let norm = 1.0 / total;
                for p in &mut partials {
                    p.amp *= norm;
                }
            }

            Self {
                partials,
                amplitude,
                stage: BankStage::Attack { t: 0.0 },
                attack_s: def.attack_s,
                release_s: def.release_s,
            }
        }

        fn note_off(&mut self) {
            let from_level = match self.stage {
                BankStage::Attack { t } => {
                    if self.attack_s > 0.0 {
                        (t / self.attack_s).clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { .. } | BankStage::Done => return,
            };
            self.stage = BankStage::Release { t: 0.0, from_level };
        }

        fn done(&self) -> bool {
            matches!(self.stage, BankStage::Done)
        }

        fn advance_ramp(&mut self, dt: f32) -> f32 {
            match &mut self.stage {
                BankStage::Attack { t } => {
                    *t += dt;
                    if self.attack_s <= 0.0 || *t >= self.attack_s {
                        self.stage = BankStage::Sustain;
                        1.0
                    } else {
                        *t / self.attack_s
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { t, from_level } => {
                    *t += dt;
                    if self.release_s <= 0.0 || *t >= self.release_s {
                        self.stage = BankStage::Done;
                        0.0
                    } else {
                        *from_level * (1.0 - (*t / self.release_s))
                    }
                }
                BankStage::Done => 0.0,
            }
        }

        fn next_sample(&mut self, dt: f32) -> f32 {
            let ramp = self.advance_ramp(dt);
            let mut acc = 0.0f32;
            let mut amp_sum = 0.0f32;
            for p in &mut self.partials {
                acc = (p.phase * TAU).sin().mul_add(p.amp, acc);
                p.phase += p.phase_step;
                if p.phase >= 1.0 {
                    p.phase -= 1.0;
                }
                p.amp *= p.decay_mul;
                amp_sum += p.amp.abs();
            }
            // A held voice whose partials have rung out frees itself; a
            // pad (zero partial decay) only ends once its release ramp
            // completes.
            if matches!(self.stage, BankStage::Sustain)
                && amp_sum * self.amplitude < PARTIAL_SILENCE_FLOOR
            {
                self.stage = BankStage::Done;
            }
            acc * self.amplitude * ramp
        }

        #[cfg(test)]
        fn envelope_level(&self) -> f32 {
            self.partials.iter().map(|p| p.amp.abs()).sum()
        }

        #[cfg(test)]
        fn partial_amps(&self) -> [f32; PARTIAL_COUNT] {
            let mut out = [0.0f32; PARTIAL_COUNT];
            for (slot, p) in out.iter_mut().zip(self.partials.iter()) {
                *slot = p.amp;
            }
            out
        }

        #[cfg(test)]
        fn in_sustain(&self) -> bool {
            matches!(self.stage, BankStage::Sustain)
        }
    }

    /// Attack ramp (seconds) wrapping a sample voice — a short swell so a
    /// re-pitched recording doesn't click on at full level (ADR-0103 §6,
    /// the partial bank's ramp shape).
    const SAMPLE_ATTACK_SECS: f32 = 0.003;
    /// Release ramp (seconds) on `note_off` for a sample voice — the
    /// damper that ends a held note faster than the sample's natural decay.
    const SAMPLE_RELEASE_SECS: f32 = 0.08;
    /// Base amplitude of a sample voice before velocity scaling. Sampled
    /// recordings already carry their own level; this trims headroom so a
    /// dense chord doesn't clip past the soft-clip.
    const SAMPLE_BASE_AMP: f32 = 0.6;

    /// A region's sustain loop in device-rate coordinates (ADR-0103 §6).
    /// The SFZ frame offsets are scaled at bank assembly by the load-time
    /// resample ratio into these fractional positions, so the kernel wrap
    /// interpolates sub-sample and rounding never lands a click. `start`
    /// and `end` index the region's device-rate PCM; the voice cycles the
    /// half-open `[start, end)` interval while it sounds.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct SampleLoop {
        start: f32,
        end: f32,
    }

    /// One region of a sampled instrument bank (ADR-0103 §5/§6): a
    /// device-rate mono recording plus the inclusive MIDI key range it
    /// covers, the inclusive velocity range it answers to, the root pitch
    /// it was recorded at (so a voice repitches by
    /// `2^((pitch − pitch_keycenter) / 12)`), and an optional sustain loop.
    /// The PCM is `Arc`'d so every region naming the same sample shares one
    /// buffer and a spawned voice holds a cheap reference, not a copy.
    #[derive(Clone, Debug)]
    struct SampleRegion {
        lokey: u8,
        hikey: u8,
        lovel: u8,
        hivel: u8,
        pitch_keycenter: u8,
        pcm: Arc<[f32]>,
        /// The sustain loop, or `None` for a full-decay region that plays
        /// once and ends when its sample runs out.
        loop_region: Option<SampleLoop>,
    }

    /// A loaded sampled-instrument bank (ADR-0103 §4/§5): the regions to
    /// select between by `(pitch, velocity)`, the name derived from the
    /// `.sfz` filename, and the total decoded PCM the bank holds resident
    /// (reported in the load reply — there is no unload in v1).
    #[derive(Debug)]
    struct SampleBank {
        name: String,
        regions: Vec<SampleRegion>,
        resident_bytes: usize,
    }

    impl SampleBank {
        /// The first region whose key and velocity ranges both contain
        /// `(pitch, velocity)`, or `None` when the note falls in a gap the
        /// bank doesn't cover (the `note_on` then drops).
        fn select(&self, pitch: u8, velocity: u8) -> Option<&SampleRegion> {
            self.regions.iter().find(|r| {
                (r.lokey..=r.hikey).contains(&pitch) && (r.lovel..=r.hivel).contains(&velocity)
            })
        }
    }

    /// The sample voice kernel (ADR-0103 §6): walk the region's device-rate
    /// PCM at a repitched rate with linear interpolation, wrapped in the
    /// same short attack / `note_off`-release ramp the partial bank uses.
    /// An unlooped region ends when its sample runs out (full-decay,
    /// piano-class sets). A looped region cycles `[loop_start, loop_end)`
    /// while it sounds — interpolating across the seam back to
    /// `loop_start` — and holds the note indefinitely, ending only once the
    /// `note_off` release ramp completes (the loop keeps cycling beneath
    /// the fade).
    #[derive(Clone, Debug)]
    struct SampleVoice {
        /// Device-rate mono PCM of the selected region, shared with the
        /// bank.
        pcm: Arc<[f32]>,
        /// Fractional read position into `pcm`, advanced by `rate` each
        /// output sample.
        pos: f32,
        /// Playback rate ratio `2^((pitch − pitch_keycenter) / 12)` — the
        /// repitch from the region's root note. The PCM is already at the
        /// device rate, so this is the only resampling the hot loop does.
        rate: f32,
        /// Velocity-scaled amplitude the interpolated sample is multiplied
        /// by.
        amplitude: f32,
        /// The sustain loop bounds (device-rate fractional positions), or
        /// `None` for an unlooped region.
        loop_region: Option<SampleLoop>,
        /// Attack / release ramp, the partial bank's shape.
        stage: BankStage,
        attack_s: f32,
        release_s: f32,
        /// Set once an unlooped region's read position walks off the end of
        /// the PCM, or the release ramp completes. A looped voice never sets
        /// it from exhaustion — it ends through the release ramp.
        finished: bool,
    }

    impl SampleVoice {
        fn new(pitch: u8, velocity: u8, region: &SampleRegion) -> Self {
            let semitones = f32::from(pitch) - f32::from(region.pitch_keycenter);
            let rate = (semitones / 12.0).exp2();
            let v = f32::from(velocity) / 127.0;
            // Drop a loop whose bounds collapsed (defensive — assembly only
            // emits `start < end`): a non-positive span has no cycle.
            let loop_region = region
                .loop_region
                .filter(|lp| lp.end > lp.start && lp.start >= 0.0);
            Self {
                pcm: Arc::clone(&region.pcm),
                pos: 0.0,
                rate,
                amplitude: SAMPLE_BASE_AMP * v,
                loop_region,
                stage: BankStage::Attack { t: 0.0 },
                attack_s: SAMPLE_ATTACK_SECS,
                release_s: SAMPLE_RELEASE_SECS,
                finished: false,
            }
        }

        fn note_off(&mut self) {
            let from_level = match self.stage {
                BankStage::Attack { t } => {
                    if self.attack_s > 0.0 {
                        (t / self.attack_s).clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { .. } | BankStage::Done => return,
            };
            self.stage = BankStage::Release { t: 0.0, from_level };
        }

        fn done(&self) -> bool {
            self.finished || matches!(self.stage, BankStage::Done)
        }

        /// Advance the attack/release ramp one sample, returning its current
        /// level — the partial bank's ramp logic over the sample voice's
        /// own attack/release times.
        fn advance_ramp(&mut self, dt: f32) -> f32 {
            match &mut self.stage {
                BankStage::Attack { t } => {
                    *t += dt;
                    if self.attack_s <= 0.0 || *t >= self.attack_s {
                        self.stage = BankStage::Sustain;
                        1.0
                    } else {
                        *t / self.attack_s
                    }
                }
                BankStage::Sustain => 1.0,
                BankStage::Release { t, from_level } => {
                    *t += dt;
                    if self.release_s <= 0.0 || *t >= self.release_s {
                        self.stage = BankStage::Done;
                        0.0
                    } else {
                        *from_level * (1.0 - (*t / self.release_s))
                    }
                }
                BankStage::Done => 0.0,
            }
        }

        // Read position and PCM lengths are bounded well below 2^24 for any
        // sane sample, so the index-to-float and float-to-index casts in the
        // looped / unlooped readers are exact and non-negative on the hot
        // path.
        fn next_sample(&mut self, dt: f32) -> f32 {
            let ramp = self.advance_ramp(dt);
            if self.finished {
                return 0.0;
            }
            let len = self.pcm.len();
            if len == 0 {
                self.finished = true;
                return 0.0;
            }
            match self.loop_region {
                Some(lp) => self.next_looped(lp, len, ramp),
                None => self.next_unlooped(len, ramp),
            }
        }

        /// The unlooped read: linear interpolation over the PCM, ending the
        /// voice once the read position walks off the end (ADR-0103 §6).
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        fn next_unlooped(&mut self, len: usize, ramp: f32) -> f32 {
            let i = self.pos.floor() as usize;
            if i >= len {
                self.finished = true;
                return 0.0;
            }
            let a = self.pcm[i];
            let b = self.pcm[(i + 1).min(len - 1)];
            let frac = self.pos - i as f32;
            let s = (b - a).mul_add(frac, a) * self.amplitude * ramp;
            self.pos += self.rate;
            if self.pos >= len as f32 {
                // The held note's sample ran out — an unlooped voice ends.
                self.finished = true;
            }
            s
        }

        /// The looped read (ADR-0103 §6): interpolate within `[loop_start,
        /// loop_end)`, wrapping back to `loop_start` at the seam so the
        /// interpolation reads `loop_start` as the post-seam neighbour and
        /// produces no discontinuity beyond interpolation error. The voice
        /// never ends from exhaustion here — only the release ramp retires
        /// it (the loop keeps cycling beneath the fade).
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        fn next_looped(&mut self, lp: SampleLoop, len: usize, ramp: f32) -> f32 {
            // `pos < loop_end <= len` holds going in, so `i` is in range.
            let i = (self.pos.floor() as usize).min(len - 1);
            let a = self.pcm[i];
            // The interpolation neighbour is the next frame — but if that
            // frame reaches or crosses `loop_end`, the loop wraps, so read
            // `loop_start` instead (the seam neighbour).
            let next_index = if (i + 1) as f32 >= lp.end {
                (lp.start.floor() as usize).min(len - 1)
            } else {
                i + 1
            };
            let b = self.pcm[next_index];
            let frac = self.pos - i as f32;
            let s = (b - a).mul_add(frac, a) * self.amplitude * ramp;

            self.pos += self.rate;
            if self.pos >= lp.end {
                // Wrap the overshoot back into the loop region. Modulo the
                // loop length so a rate larger than the span still lands in
                // `[loop_start, loop_end)` in O(1).
                let loop_len = lp.end - lp.start;
                let over = self.pos - lp.end;
                let wrapped = (over / loop_len).floor().mul_add(-loop_len, over);
                self.pos = lp.start + wrapped;
            }
            s
        }
    }

    /// Voice kernel — one of the three synthesis models, selected by the
    /// instrument at `note_on`: a built-in oscillator or partial-bank
    /// patch, or a loaded sampled instrument (ADR-0103 §6).
    #[derive(Clone, Debug)]
    enum VoiceKernel {
        Oscillator(OscVoice),
        PartialBank(PartialBankVoice),
        Sample(SampleVoice),
    }

    /// Build the kernel for a built-in instrument patch (oscillator or
    /// partial bank). Split out of [`Voice`] so the `note_on` path can
    /// resolve a built-in or a loaded sample bank into a `VoiceKernel`
    /// before the steal / dedup bookkeeping, then stamp one `Voice`.
    fn build_builtin_kernel(
        sender_mailbox: MailboxId,
        instrument_id: u8,
        pitch: u8,
        velocity: u8,
        def: &InstrumentDef,
        sample_rate: f32,
    ) -> VoiceKernel {
        match def.voice {
            VoiceDef::Oscillator { wave, adsr } => {
                let seed = voice_seed(sender_mailbox, instrument_id, pitch);
                let mut osc =
                    OscVoice::new(pitch, velocity, wave, adsr, def.base_amp, sample_rate, seed);
                if let Some(sweep) = def.pitch_sweep {
                    osc = osc.with_pitch_sweep(sweep, sample_rate);
                }
                VoiceKernel::Oscillator(osc)
            }
            VoiceDef::PartialBank(bank) => VoiceKernel::PartialBank(PartialBankVoice::new(
                pitch,
                velocity,
                &bank,
                def.base_amp,
                sample_rate,
            )),
        }
    }

    /// A single sounding voice: the routing key (`sender_mailbox`,
    /// `instrument_id`, `pitch`) plus the kernel that renders it. No longer
    /// `Copy` — a sample voice holds a reference-counted PCM handle
    /// (ADR-0103 §6) — but the pool was never structurally dependent on
    /// `Copy`; it stays a flat `Vec<Voice>` mutated by `swap_remove` /
    /// `push`.
    ///
    /// `seq` is a monotonically increasing counter stamped at allocation,
    /// used by voice-steal to locate the oldest voice regardless of the
    /// pool's current order (which `swap_remove` scrambles).
    #[derive(Clone, Debug)]
    struct Voice {
        sender_mailbox: MailboxId,
        instrument_id: u8,
        pitch: u8,
        seq: u64,
        kernel: VoiceKernel,
    }

    impl Voice {
        fn note_off(&mut self) {
            match &mut self.kernel {
                VoiceKernel::Oscillator(v) => v.note_off(),
                VoiceKernel::PartialBank(v) => v.note_off(),
                VoiceKernel::Sample(v) => v.note_off(),
            }
        }

        fn done(&self) -> bool {
            match &self.kernel {
                VoiceKernel::Oscillator(v) => v.done(),
                VoiceKernel::PartialBank(v) => v.done(),
                VoiceKernel::Sample(v) => v.done(),
            }
        }

        fn next_sample(&mut self, dt: f32) -> f32 {
            match &mut self.kernel {
                VoiceKernel::Oscillator(v) => v.next_sample(dt),
                VoiceKernel::PartialBank(v) => v.next_sample(dt),
                VoiceKernel::Sample(v) => v.next_sample(dt),
            }
        }
    }

    /// Fade state of a [`TrackVoice`]. A track plays at full level until
    /// `stop_track` arms a short linear fade-out; `remaining` counts down
    /// per output sample and the track retires when it hits zero (ADR-0103
    /// §3).
    #[derive(Clone, Debug)]
    enum TrackFade {
        Playing,
        FadingOut { remaining: u32, total: u32 },
    }

    /// One playing track in the dedicated mixer lane (ADR-0103 §3). Holds
    /// the `Arc`'d device-rate mono PCM, a position walk, per-track gain,
    /// loop flag, and fade state. A track neither counts against
    /// `MAX_VOICES` nor participates in voice-steal — a music bed must not
    /// be evicted by a note flurry. Keyed by `(sender_mailbox, lane,
    /// namespace, path)`, mirroring the voice key plus the caller-supplied
    /// `lane` that disambiguates senders sharing a source mailbox.
    struct TrackVoice {
        sender_mailbox: MailboxId,
        lane: Option<String>,
        namespace: String,
        path: String,
        pcm: Arc<[f32]>,
        position: usize,
        gain: f32,
        looping: bool,
        fade: TrackFade,
        done: bool,
    }

    impl TrackVoice {
        fn new(
            sender_mailbox: MailboxId,
            lane: Option<String>,
            namespace: String,
            path: String,
            pcm: Arc<[f32]>,
            gain: f32,
            looping: bool,
        ) -> Self {
            Self {
                sender_mailbox,
                lane,
                namespace,
                path,
                pcm,
                position: 0,
                gain,
                looping,
                fade: TrackFade::Playing,
                done: false,
            }
        }

        /// True when this event's key matches the track's
        /// `(sender_mailbox, lane, namespace, path)`.
        fn matches(
            &self,
            sender_mailbox: MailboxId,
            lane: Option<&String>,
            namespace: &str,
            path: &str,
        ) -> bool {
            self.sender_mailbox == sender_mailbox
                && self.lane.as_ref() == lane
                && self.namespace == namespace
                && self.path == path
        }

        /// Arm the fade-out. Idempotent — a second `stop` while already
        /// fading keeps the first fade's progress.
        fn stop(&mut self, fade_samples: u32) {
            if matches!(self.fade, TrackFade::Playing) {
                let total = fade_samples.max(1);
                self.fade = TrackFade::FadingOut {
                    remaining: total,
                    total,
                };
            }
        }

        fn done(&self) -> bool {
            self.done
        }

        /// Render this track's next sample (already gained + faded) and
        /// advance the position. Returns `0.0` once retired; an empty PCM
        /// buffer retires immediately.
        fn next_sample(&mut self) -> f32 {
            if self.done || self.pcm.is_empty() {
                self.done = true;
                return 0.0;
            }
            let fade_mul = match &mut self.fade {
                TrackFade::Playing => 1.0,
                TrackFade::FadingOut { remaining, total } => {
                    if *remaining == 0 {
                        self.done = true;
                        return 0.0;
                    }
                    // `remaining` / `total` are small fade-window counts —
                    // the ratio is exact in f32.
                    #[allow(clippy::cast_precision_loss)]
                    let mul = *remaining as f32 / *total as f32;
                    *remaining -= 1;
                    mul
                }
            };
            let sample = self.pcm[self.position] * self.gain * fade_mul;
            self.position += 1;
            if self.position >= self.pcm.len() {
                if self.looping {
                    self.position = 0;
                } else {
                    self.done = true;
                }
            }
            sample
        }
    }

    /// Whole-process synth state. Lives on the cpal callback thread;
    /// the cap communicates via the event queue.
    struct Synth {
        events: Arc<ArrayQueue<AudioEvent>>,
        voices: Vec<Voice>,
        /// Track playback lane (ADR-0103 §3) — separate from `voices` so a
        /// track is never counted against `MAX_VOICES` nor voice-stolen.
        tracks: Vec<TrackVoice>,
        /// Loaded sampled-instrument banks (ADR-0103 §4), appended in load
        /// order. Index `i` is `instrument_id` `BUILTINS.len() + i`, so a
        /// `note_on` whose id walks past the built-ins indexes here. The cap
        /// assigns ids the same way, so the two stay in lockstep.
        banks: Vec<Arc<SampleBank>>,
        sample_rate: f32,
        master_gain: f32,
        /// Monotonically increasing counter stamped into each `Voice::seq`
        /// at allocation. Voice-steal uses the minimum value to locate the
        /// oldest voice regardless of pool order.
        next_seq: u64,
        /// Running output-frame counter (ADR-0104). Advanced by the frame
        /// count of every `fill`; the timebase scheduled events are placed
        /// against and fire from. Callback-owned, so no locking.
        frame_clock: u64,
        /// Pending scheduled note events ordered by due frame (ADR-0104),
        /// a min-heap via `Reverse`. `fill` pops the events that fall on
        /// each frame and routes them through the note-on / note-off paths.
        scheduled: BinaryHeap<Reverse<ScheduledEntry>>,
        /// Monotonic stamp threaded into each `ScheduledEntry::seq` so that
        /// events on the same due frame fire in batch-arrival order.
        next_schedule_seq: u64,
    }

    impl Synth {
        fn new(events: Arc<ArrayQueue<AudioEvent>>, sample_rate: f32) -> Self {
            Self {
                events,
                voices: Vec::with_capacity(MAX_VOICES),
                tracks: Vec::new(),
                banks: Vec::new(),
                sample_rate,
                master_gain: 1.0,
                next_seq: 0,
                frame_clock: 0,
                scheduled: BinaryHeap::new(),
                next_schedule_seq: 0,
            }
        }

        /// Resolve a loaded sample bank by `instrument_id`, returning a
        /// cheap `Arc` clone (or `None` for an id still inside the built-in
        /// range or past the loaded banks). The `note_on` path falls back
        /// to this when `instrument_by_id` misses.
        fn bank_for(&self, instrument_id: u8) -> Option<Arc<SampleBank>> {
            let index = (instrument_id as usize).checked_sub(BUILTINS.len())?;
            self.banks.get(index).map(Arc::clone)
        }

        /// Number of output samples in the `stop_track` fade-out at this
        /// device rate.
        fn fade_samples(&self) -> u32 {
            // Fade window is a few milliseconds at audio rates — well
            // within u32 and non-negative.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = (TRACK_FADE_SECS * self.sample_rate) as u32;
            n
        }

        /// Admit a `note_on`: resolve its kernel (a built-in patch, or — when
        /// the id walks past the built-ins — a loaded sample bank's region
        /// selected by `(pitch, velocity)`), then steal the oldest voice if
        /// at capacity, replace any voice already on the same key, and push.
        /// A miss on both kernel sources (unknown id, or a bank with no
        /// region covering the note) warn-drops without touching the pool
        /// (ADR-0103 §6).
        fn trigger_note_on(
            &mut self,
            sender_mailbox: MailboxId,
            pitch: u8,
            velocity: u8,
            instrument_id: u8,
        ) {
            let kernel = if let Some(def) = instrument_by_id(instrument_id) {
                Some(build_builtin_kernel(
                    sender_mailbox,
                    instrument_id,
                    pitch,
                    velocity,
                    def,
                    self.sample_rate,
                ))
            } else {
                self.bank_for(instrument_id).and_then(|bank| {
                    bank.select(pitch, velocity).map(|region| {
                        VoiceKernel::Sample(SampleVoice::new(pitch, velocity, region))
                    })
                })
            };
            let Some(kernel) = kernel else {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    instrument_id,
                    pitch,
                    velocity,
                    "note_on: no instrument / region for id, dropping",
                );
                return;
            };
            if self.voices.len() >= MAX_VOICES {
                // Evict the oldest (minimum-seq) voice. swap_remove is O(1)
                // and safe here because the pool is non-empty at capacity.
                if let Some(oldest_idx) = self
                    .voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.seq)
                    .map(|(i, _)| i)
                {
                    self.voices.swap_remove(oldest_idx);
                }
            }
            if let Some(existing) = self.voices.iter().position(|v| {
                v.sender_mailbox == sender_mailbox
                    && v.instrument_id == instrument_id
                    && v.pitch == pitch
            }) {
                self.voices.swap_remove(existing);
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            self.voices.push(Voice {
                sender_mailbox,
                instrument_id,
                pitch,
                seq,
                kernel,
            });
        }

        /// Release the voice matching `(sender_mailbox, instrument_id,
        /// pitch)`, if one is sounding. A miss is a silent no-op (a late or
        /// unmatched note-off), matching the immediate `note_off` path.
        /// Shared by the queue-drained note-off and the scheduled note-off.
        fn trigger_note_off(&mut self, sender_mailbox: MailboxId, pitch: u8, instrument_id: u8) {
            if let Some(v) = self.voices.iter_mut().find(|v| {
                v.sender_mailbox == sender_mailbox
                    && v.instrument_id == instrument_id
                    && v.pitch == pitch
            }) {
                v.note_off();
            }
        }

        /// Fire one scheduled note event through the same paths the
        /// immediate mail would take (ADR-0104).
        fn fire_scheduled(&mut self, sender_mailbox: MailboxId, note: &ScheduledNote) {
            match *note {
                ScheduledNote::On {
                    pitch,
                    velocity,
                    instrument_id,
                } => self.trigger_note_on(sender_mailbox, pitch, velocity, instrument_id),
                ScheduledNote::Off {
                    pitch,
                    instrument_id,
                } => self.trigger_note_off(sender_mailbox, pitch, instrument_id),
            }
        }

        fn drain_events(&mut self) {
            while let Some(ev) = self.events.pop() {
                match ev {
                    AudioEvent::NoteOn {
                        sender_mailbox,
                        pitch,
                        velocity,
                        instrument_id,
                    } => self.trigger_note_on(sender_mailbox, pitch, velocity, instrument_id),
                    AudioEvent::NoteOff {
                        sender_mailbox,
                        pitch,
                        instrument_id,
                    } => self.trigger_note_off(sender_mailbox, pitch, instrument_id),
                    AudioEvent::SetMasterGain { gain } => {
                        self.master_gain = gain.clamp(0.0, 1.0);
                    }
                    AudioEvent::TrackStart {
                        sender_mailbox,
                        lane,
                        namespace,
                        path,
                        pcm,
                        gain,
                        looping,
                    } => {
                        self.start_track(sender_mailbox, lane, namespace, path, pcm, gain, looping);
                    }
                    AudioEvent::TrackStop {
                        sender_mailbox,
                        lane,
                        namespace,
                        path,
                    } => self.stop_track(sender_mailbox, lane.as_ref(), &namespace, &path),
                    AudioEvent::RegisterInstrument { id, bank } => {
                        // Banks arrive in load order on this single-producer
                        // FIFO, and the cap assigns ids from `BUILTINS.len()`
                        // upward in the same order, so the new bank's index
                        // is exactly `id - BUILTINS.len()` == current length.
                        // A mismatch is a wiring bug, not a runtime input —
                        // log it but still append so lookups stay dense.
                        let expected = BUILTINS.len() + self.banks.len();
                        if id as usize != expected {
                            tracing::warn!(
                                target: "aether_substrate::audio",
                                id,
                                expected,
                                "register_instrument: id out of step with load order",
                            );
                        }
                        self.banks.push(bank);
                    }
                    AudioEvent::Schedule {
                        sender_mailbox,
                        events,
                    } => {
                        // Offsets are relative to receipt at the callback —
                        // the current frame clock (this drain runs at block
                        // start). Every event in the batch shares this
                        // anchor, so simultaneous events stay simultaneous.
                        for event in events {
                            let due_frame = self.frame_clock
                                + millis_to_frames(event.at_millis, self.sample_rate);
                            let seq = self.next_schedule_seq;
                            self.next_schedule_seq += 1;
                            self.scheduled.push(Reverse(ScheduledEntry {
                                due_frame,
                                seq,
                                sender_mailbox,
                                note: event.event,
                            }));
                        }
                    }
                }
            }
        }

        /// Start (or restart) a track in the lane. Re-playing the same
        /// `(sender_mailbox, lane, namespace, path)` key drops the existing
        /// track first, so a key never stacks.
        #[allow(clippy::too_many_arguments)]
        fn start_track(
            &mut self,
            sender_mailbox: MailboxId,
            lane: Option<String>,
            namespace: String,
            path: String,
            pcm: Arc<[f32]>,
            gain: f32,
            looping: bool,
        ) {
            if let Some(i) = self
                .tracks
                .iter()
                .position(|t| t.matches(sender_mailbox, lane.as_ref(), &namespace, &path))
            {
                self.tracks.swap_remove(i);
            }
            self.tracks.push(TrackVoice::new(
                sender_mailbox,
                lane,
                namespace,
                path,
                pcm,
                gain,
                looping,
            ));
        }

        /// Arm the fade-out on the track at this key, if one is playing.
        fn stop_track(
            &mut self,
            sender_mailbox: MailboxId,
            lane: Option<&String>,
            namespace: &str,
            path: &str,
        ) {
            let fade = self.fade_samples();
            if let Some(t) = self
                .tracks
                .iter_mut()
                .find(|t| t.matches(sender_mailbox, lane, namespace, path))
            {
                t.stop(fade);
            }
        }

        fn fill(&mut self, buffer: &mut [f32], channels: usize) {
            self.drain_events();
            let dt = 1.0 / self.sample_rate;
            let frames = buffer.len() / channels.max(1);
            for frame in 0..frames {
                // Fire every scheduled event due on or before this frame
                // before rendering it, so a scheduled note's voice is alive
                // for the sample it falls on — sample-accurate by
                // construction (ADR-0104).
                let absolute = self.frame_clock + frame as u64;
                loop {
                    match self.scheduled.peek() {
                        Some(Reverse(top)) if top.due_frame <= absolute => {}
                        _ => break,
                    }
                    let Reverse(entry) = self
                        .scheduled
                        .pop()
                        .expect("peeked entry is present this iteration");
                    self.fire_scheduled(entry.sender_mailbox, &entry.note);
                }
                let mut sample = 0.0f32;
                for voice in &mut self.voices {
                    sample += voice.next_sample(dt);
                }
                // Tracks mix in their own lane, summed after the voices
                // and before master gain + the soft clip (ADR-0103 §3).
                for track in &mut self.tracks {
                    sample += track.next_sample();
                }
                sample *= self.master_gain;
                sample = sample.tanh();
                let start = frame * channels;
                for ch in 0..channels {
                    buffer[start + ch] = sample;
                }
            }
            // Advance the clock by this block so the next drain anchors
            // scheduled offsets against the right receipt frame (ADR-0104).
            self.frame_clock += frames as u64;
            let mut i = 0;
            while i < self.voices.len() {
                if self.voices[i].done() {
                    self.voices.swap_remove(i);
                } else {
                    i += 1;
                }
            }
            let mut t = 0;
            while t < self.tracks.len() {
                if self.tracks[t].done() {
                    self.tracks.swap_remove(t);
                } else {
                    t += 1;
                }
            }
        }

        #[cfg(test)]
        fn voice_count(&self) -> usize {
            self.voices.len()
        }

        #[cfg(test)]
        fn has_voice_with_pitch(&self, pitch: u8) -> bool {
            self.voices.iter().any(|v| v.pitch == pitch)
        }

        #[cfg(test)]
        fn master_gain_value(&self) -> f32 {
            self.master_gain
        }

        #[cfg(test)]
        fn track_count(&self) -> usize {
            self.tracks.len()
        }

        #[cfg(test)]
        fn bank_count(&self) -> usize {
            self.banks.len()
        }

        #[cfg(test)]
        fn scheduled_count(&self) -> usize {
            self.scheduled.len()
        }
    }

    /// Handle to a running cpal pipeline. Lives on the audio worker
    /// thread for the entire run — `cpal::Stream` is `!Send` on macOS,
    /// so the stream is constructed on, owned by, and dropped from the
    /// same thread. Dropping the pipeline silences every voice and tears
    /// down the cpal stream.
    struct AudioPipeline {
        sender: AudioEventSender,
        /// The device output rate the synth runs at. The cap reads it back
        /// (via the init channel) as the resample target for track decode
        /// (ADR-0103 §1) — decode happens on the dispatcher, not here.
        sample_rate: u32,
        _stream: cpal::Stream,
    }

    #[derive(Debug)]
    enum AudioBuildError {
        NoDevice,
        RateUnsupported(u32),
        ConfigQuery(String),
        StreamBuild(String),
        StreamPlay(String),
    }

    impl fmt::Display for AudioBuildError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::NoDevice => write!(f, "no default audio output device"),
                Self::RateUnsupported(r) => write!(f, "requested sample rate {r} Hz unsupported"),
                Self::ConfigQuery(e) => write!(f, "config query failed: {e}"),
                Self::StreamBuild(e) => write!(f, "stream build failed: {e}"),
                Self::StreamPlay(e) => write!(f, "stream play failed: {e}"),
            }
        }
    }

    fn try_build_pipeline(
        requested_sample_rate: Option<u32>,
    ) -> Result<AudioPipeline, AudioBuildError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioBuildError::NoDevice)?;

        let config = match requested_sample_rate {
            Some(rate) => {
                find_config_for_rate(&device, rate).ok_or(AudioBuildError::RateUnsupported(rate))?
            }
            None => device
                .default_output_config()
                .map_err(|e| AudioBuildError::ConfigQuery(e.to_string()))?
                .config(),
        };

        let sample_rate = config.sample_rate;
        let channels = config.channels;

        let (sender, queue) = new_event_channel();
        // Audio sample rates are bounded well below 2^24 — exact in f32.
        #[allow(clippy::cast_precision_loss)]
        let mut synth = Synth::new(queue, sample_rate as f32);

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    synth.fill(data, channels as usize);
                },
                |err| {
                    tracing::warn!(
                        target: "aether_substrate::audio",
                        error = %err,
                        "cpal stream error",
                    );
                },
                None,
            )
            .map_err(|e| AudioBuildError::StreamBuild(e.to_string()))?;

        stream
            .play()
            .map_err(|e| AudioBuildError::StreamPlay(e.to_string()))?;

        tracing::info!(
            target: "aether_substrate::audio",
            sample_rate,
            channels,
            instruments = builtin_count(),
            builtin_names = ?builtin_names(),
            "audio pipeline started",
        );

        Ok(AudioPipeline {
            sender,
            sample_rate,
            _stream: stream,
        })
    }

    fn find_config_for_rate(device: &cpal::Device, rate: u32) -> Option<cpal::StreamConfig> {
        let configs = device.supported_output_configs().ok()?;
        for cfg in configs {
            let min = cfg.min_sample_rate();
            let max = cfg.max_sample_rate();
            if rate >= min && rate <= max {
                return Some(cfg.with_sample_rate(rate).config());
            }
        }
        None
    }

    /// Extract the sender's mailbox id for voice-table keying. Component
    /// senders come through as `EngineMailbox { mailbox_id }`; Claude
    /// sessions and substrate-internal pushes (which shouldn't reach the
    /// audio cap in practice) collapse to id `0`, sharing one voice
    /// slot per (instrument, pitch).
    /// The track/voice key's sender component, read from the mail
    /// envelope's reply target. Only an `EngineMailbox` source carries a
    /// distinct id; every other source — MCP sessions, substrate-internal
    /// mail — collapses to `MailboxId(0)`. Callers that share this id
    /// disambiguate their tracks with the payload's `lane` field rather
    /// than the sender (ADR-0103 keying).
    fn sender_mailbox_id(sender: Source) -> MailboxId {
        match sender.addr {
            SourceAddr::EngineMailbox { mailbox_id, .. } => mailbox_id,
            _ => MailboxId(0),
        }
    }

    /// `aether.audio` mailbox cap. Holds the producer side of the synth
    /// event queue (the crate-internal `AudioEventSender`), the audio
    /// worker thread that owns the [`cpal::Stream`] (see module-level
    /// "per-cap audio worker" docs for the `!Send` rationale), and a
    /// shutdown channel that signals the worker to exit on drop.
    ///
    /// `sender` is `None` when the cpal pipeline isn't running
    /// (`AETHER_AUDIO_DISABLE=1`, no audio device, init failure). In
    /// that mode `NoteOn` / `NoteOff` no-op and `SetMasterGain` replies
    /// `Err`.
    ///
    /// Issue 629 / Phase B: `thread` and `shutdown` are
    /// plain fields. Pre-Phase-A they sat behind a `Mutex<AudioTeardown>`
    /// so `Drop::drop(&mut self)` could `.take()` them while handlers
    /// ran with `&self` (Arc-shared). Post-Phase-A the dispatcher owns
    /// the cap as `Box<A>` and `Drop` runs with exclusive `&mut self`,
    /// so the wrapping mutex retires.
    /// A `play_track` request parked while its `aether.fs.read` is in
    /// flight (ADR-0103 §2). Keyed in [`AudioCapability::pending_tracks`]
    /// by the echoed `(namespace, path)` the `ReadResult` carries; the
    /// original requester's reply route + the synth-side track key live
    /// here until the bytes land.
    struct PendingTrack {
        /// The original `play_track` requester — the `PlayTrackResult`
        /// reply routes here across the fs round-trip + decode.
        source: Source,
        /// The synth-side track key's sender component, baked into the
        /// `TrackStart` event so the lane keys by `(sender, lane,
        /// namespace, path)` while the fs correlation keys by
        /// `(namespace, path)`.
        sender_mailbox: MailboxId,
        /// The caller-supplied lane that disambiguates senders sharing a
        /// source mailbox; part of the synth-side track key.
        lane: Option<String>,
        gain: f32,
        looping: bool,
    }

    /// Completion context the `play_track` decode dispatch carries so the
    /// `#[handler(task)]` arm can build the `TrackStart` event + the reply
    /// without re-deriving anything (ADR-0093 §5). The worker produces the
    /// decoded PCM; this carries the synth key + play parameters alongside.
    struct TrackDecodeContext {
        sender_mailbox: MailboxId,
        lane: Option<String>,
        namespace: String,
        path: String,
        gain: f32,
        looping: bool,
    }

    /// Output of the decode dispatch worker — the resampled mono PCM, or
    /// the decode failure to relay as `PlayTrackResult::Err`.
    type DecodeOutput = Result<Vec<f32>, DecodeError>;

    /// A `load_instrument` request parked while its `.sfz` `aether.fs.read`
    /// is in flight (ADR-0103 §2/§5). Keyed in
    /// [`AudioCapability::pending_instruments`] by the echoed
    /// `(namespace, path)` of the `.sfz`. Only the original requester's
    /// reply route lives here — the namespace / path come back on the
    /// `ReadResult`, and the bank's name is derived from the `.sfz` path.
    struct PendingInstrument {
        source: Source,
    }

    /// One unique sample a bank assembly is fetching: the path as written
    /// in the `.sfz` (resolved against `default_path`), the fs path it is
    /// read from (joined with the `.sfz`'s own directory), and its bytes
    /// once the `aether.fs.read` lands.
    struct SampleSlot {
        sample_rel: String,
        fs_path: String,
        bytes: Option<Vec<u8>>,
    }

    /// A bank load in progress: the `.sfz` parsed into regions, fanning out
    /// one `aether.fs.read` per unique referenced sample, assembling when
    /// the last reply lands (ADR-0103 §2). Keyed in
    /// [`AudioCapability::assemblies`] by a minted id; the per-sample reads
    /// correlate back to it through [`AudioCapability::pending_samples`].
    struct BankAssembly {
        /// The original `load_instrument` requester — the
        /// `LoadInstrumentResult` reply routes here.
        source: Source,
        /// The fs namespace the `.sfz` and its samples live in (shared).
        namespace: String,
        /// The `.sfz` path — echoed on an `Err` reply for correlation.
        sfz_path: String,
        /// Bank name, derived from the `.sfz` filename stem.
        name: String,
        /// The parsed regions; each names a `sample_rel` resolved at
        /// assembly time to its decoded PCM.
        regions: Vec<SfzRegion>,
        /// The unique samples, fetched in parallel.
        samples: Vec<SampleSlot>,
        /// How many samples are still missing their bytes; the bank
        /// assembles when this reaches zero.
        remaining: usize,
    }

    /// Completion context the bank-assembly dispatch carries so the
    /// `#[handler(task)]` arm can build the `Err` reply (`Ok` carries the
    /// assembled bank's own name / id / bytes). Mirrors
    /// [`TrackDecodeContext`] for the load path.
    struct BankAssemblyContext {
        namespace: String,
        path: String,
    }

    /// Output of the bank-assembly dispatch worker — the assembled,
    /// device-rate bank behind an `Arc`, or a human-readable decode failure
    /// to relay as `LoadInstrumentResult::Err`.
    type BankAssemblyOutput = Result<Arc<SampleBank>, String>;

    pub struct AudioCapability {
        sender: Option<AudioEventSender>,
        /// Device output rate, captured at boot — the resample target for
        /// track decode (ADR-0103 §1). `None` in nop mode (no pipeline).
        sample_rate: Option<f32>,
        /// `play_track` requests awaiting their `aether.fs.read` reply,
        /// keyed by the echoed `(namespace, path)`. A `VecDeque` per key so
        /// two concurrent plays of the same path correlate FIFO rather than
        /// clobbering each other.
        pending_tracks: HashMap<(String, String), VecDeque<PendingTrack>>,
        /// `load_instrument` requests awaiting their `.sfz` read, keyed by
        /// the echoed `(namespace, path)` of the `.sfz` (ADR-0103 §5).
        pending_instruments: HashMap<(String, String), VecDeque<PendingInstrument>>,
        /// Bank loads whose `.sfz` has parsed and whose sample reads are in
        /// flight, keyed by a minted assembly id.
        assemblies: HashMap<u64, BankAssembly>,
        /// Sample reads in flight, keyed by the echoed `(namespace, fs_path)`
        /// to the assembly id(s) awaiting that sample (FIFO across banks
        /// that happen to share a sample path).
        pending_samples: HashMap<(String, String), VecDeque<u64>>,
        /// Monotonic source of [`BankAssembly`] keys.
        next_assembly_id: u64,
        /// Next instrument id to assign a loaded bank — starts at
        /// `BUILTINS.len()` and counts up in load order (ADR-0103 §4),
        /// matching the synth's append-only bank table.
        next_instrument_id: u8,
        thread: Option<JoinHandle<()>>,
        shutdown: Option<mpsc::Sender<()>>,
    }

    impl AudioCapability {
        fn nop() -> Self {
            Self {
                sender: None,
                sample_rate: None,
                pending_tracks: HashMap::new(),
                pending_instruments: HashMap::new(),
                assemblies: HashMap::new(),
                pending_samples: HashMap::new(),
                next_assembly_id: 0,
                next_instrument_id: builtin_id_ceiling(),
                thread: None,
                shutdown: None,
            }
        }

        /// Pop the oldest `play_track` parked under `(namespace, path)` —
        /// the FIFO correlation for the `aether.fs.read` reply. Removes the
        /// key's queue when it empties.
        fn take_pending(&mut self, namespace: &str, path: &str) -> Option<PendingTrack> {
            let key = (namespace.to_owned(), path.to_owned());
            let queue = self.pending_tracks.get_mut(&key)?;
            let pending = queue.pop_front();
            if queue.is_empty() {
                self.pending_tracks.remove(&key);
            }
            pending
        }

        /// Pop the oldest `load_instrument` parked under the `.sfz`'s
        /// `(namespace, path)`. Sibling of [`Self::take_pending`].
        fn take_pending_instrument(
            &mut self,
            namespace: &str,
            path: &str,
        ) -> Option<PendingInstrument> {
            let key = (namespace.to_owned(), path.to_owned());
            let queue = self.pending_instruments.get_mut(&key)?;
            let pending = queue.pop_front();
            if queue.is_empty() {
                self.pending_instruments.remove(&key);
            }
            pending
        }

        /// Pop the oldest assembly awaiting a sample read at
        /// `(namespace, fs_path)`.
        fn take_pending_sample(&mut self, namespace: &str, path: &str) -> Option<u64> {
            let key = (namespace.to_owned(), path.to_owned());
            let queue = self.pending_samples.get_mut(&key)?;
            let id = queue.pop_front();
            if queue.is_empty() {
                self.pending_samples.remove(&key);
            }
            id
        }

        /// Dispatch a track's decode off the realtime path (ADR-0093),
        /// pinning the deferred `PlayTrackResult` to the original
        /// `play_track` caller. Split out of `on_read_result` so the one
        /// handler can route three fetch paths.
        fn start_track_decode(
            &mut self,
            ctx: &mut NativeCtx<'_, Manual>,
            pending: &PendingTrack,
            namespace: String,
            path: String,
            bytes: Vec<u8>,
        ) {
            let Some(target_rate_f32) = self.sample_rate else {
                ctx.reply_to(
                    pending.source,
                    &PlayTrackResult::Err {
                        namespace,
                        path,
                        lane: pending.lane.clone(),
                        error: "audio pipeline not initialised on this desktop substrate"
                            .to_owned(),
                    },
                );
                return;
            };
            // Device rates are small positive integers — the round trip back
            // through u32 is exact.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let target_rate = target_rate_f32 as u32;

            let context = TrackDecodeContext {
                sender_mailbox: pending.sender_mailbox,
                lane: pending.lane.clone(),
                namespace,
                path,
                gain: pending.gain,
                looping: pending.looping,
            };
            // Bridge the hold from this (fs-reply) turn into the decode
            // dispatch, pinning the reply to the original `play_track` caller.
            let hold = ctx.acquire_settlement_hold();
            ctx.dispatch_blocking_resumed_with::<DecodeOutput, _, _>(
                hold,
                pending.source,
                context,
                move || decode_wav_to_mono(&bytes, target_rate),
            );
        }

        /// The `.sfz` bytes landed: parse the SFZ subset and fan out one
        /// `aether.fs.read` per unique referenced sample (ADR-0103 §5). A
        /// bad UTF-8 / parse replies `Err` immediately; otherwise a
        /// [`BankAssembly`] is parked until the sample reads complete.
        fn on_sfz_loaded(
            &mut self,
            ctx: &mut NativeCtx<'_, Manual>,
            pending: &PendingInstrument,
            namespace: String,
            path: String,
            bytes: &[u8],
        ) {
            let Ok(text) = from_utf8(bytes) else {
                ctx.reply_to(
                    pending.source,
                    &LoadInstrumentResult::Err {
                        namespace,
                        path,
                        error: "sfz file is not valid UTF-8".to_owned(),
                    },
                );
                return;
            };
            let spec = match parse_sfz(text) {
                Ok(spec) => spec,
                Err(e) => {
                    ctx.reply_to(
                        pending.source,
                        &LoadInstrumentResult::Err {
                            namespace,
                            path,
                            error: format!("sfz parse failed: {e}"),
                        },
                    );
                    return;
                }
            };

            let dir = sfz_dir(&path);
            let name = bank_name_from_path(&path);
            let samples: Vec<SampleSlot> = spec
                .sample_paths()
                .into_iter()
                .map(|rel| SampleSlot {
                    fs_path: join_fs(dir, &rel),
                    sample_rel: rel,
                    bytes: None,
                })
                .collect();
            // `parse_sfz` guarantees at least one region with a sample, so
            // `samples` is non-empty.
            let remaining = samples.len();
            let assembly_id = self.next_assembly_id;
            self.next_assembly_id += 1;

            let fs_paths: Vec<String> = samples.iter().map(|s| s.fs_path.clone()).collect();
            self.assemblies.insert(
                assembly_id,
                BankAssembly {
                    source: pending.source,
                    namespace: namespace.clone(),
                    sfz_path: path,
                    name,
                    regions: spec.regions,
                    samples,
                    remaining,
                },
            );

            // Address the fs cap through the lineage-correct resolver
            // (ADR-0099); `send` propagates this handler's chain by default
            // so each `ReadResult` settles back into it.
            let fs = ctx.actor::<FsCapability>();
            for fs_path in fs_paths {
                self.pending_samples
                    .entry((namespace.clone(), fs_path.clone()))
                    .or_default()
                    .push_back(assembly_id);
                let read = Read {
                    namespace: namespace.clone(),
                    path: fs_path,
                };
                fs.send(&read);
            }
        }

        /// A sample's bytes landed: store them against its slot and, once
        /// the last sample is in, dispatch the decode + assembly off the
        /// realtime path (ADR-0093 / ADR-0103 §6). A late / orphan reply
        /// (its assembly already failed) is dropped.
        fn on_sample_loaded(
            &mut self,
            ctx: &mut NativeCtx<'_, Manual>,
            assembly_id: u64,
            fs_path: &str,
            bytes: Vec<u8>,
        ) {
            let ready = {
                let Some(assembly) = self.assemblies.get_mut(&assembly_id) else {
                    return;
                };
                if let Some(slot) = assembly
                    .samples
                    .iter_mut()
                    .find(|s| s.fs_path == fs_path && s.bytes.is_none())
                {
                    slot.bytes = Some(bytes);
                    assembly.remaining = assembly.remaining.saturating_sub(1);
                }
                assembly.remaining == 0
            };
            if !ready {
                return;
            }

            let assembly = self
                .assemblies
                .remove(&assembly_id)
                .expect("assembly present — checked above");
            let Some(target_rate_f32) = self.sample_rate else {
                ctx.reply_to(
                    assembly.source,
                    &LoadInstrumentResult::Err {
                        namespace: assembly.namespace,
                        path: assembly.sfz_path,
                        error: "audio pipeline not initialised on this desktop substrate"
                            .to_owned(),
                    },
                );
                return;
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let target_rate = target_rate_f32 as u32;

            let BankAssembly {
                source,
                namespace,
                sfz_path,
                name,
                regions,
                samples,
                ..
            } = assembly;
            let sample_bytes: Vec<(String, Vec<u8>)> = samples
                .into_iter()
                .map(|s| (s.sample_rel, s.bytes.unwrap_or_default()))
                .collect();
            let context = BankAssemblyContext {
                namespace,
                path: sfz_path,
            };
            let hold = ctx.acquire_settlement_hold();
            ctx.dispatch_blocking_resumed_with::<BankAssemblyOutput, _, _>(
                hold,
                source,
                context,
                move || assemble_bank(name, &regions, &sample_bytes, target_rate),
            );
        }

        /// Abandon a bank load whose sample read failed: reply `Err` to the
        /// original requester and discard the partial assembly (ADR-0103
        /// §2). Sibling sample reads still in flight prune from the pending
        /// table; their replies will find no assembly and drop.
        fn fail_assembly(
            &mut self,
            ctx: &mut NativeCtx<'_, Manual>,
            assembly_id: u64,
            error: String,
        ) {
            let Some(assembly) = self.assemblies.remove(&assembly_id) else {
                return;
            };
            ctx.reply_to(
                assembly.source,
                &LoadInstrumentResult::Err {
                    namespace: assembly.namespace,
                    path: assembly.sfz_path,
                    error,
                },
            );
            for queue in self.pending_samples.values_mut() {
                queue.retain(|id| *id != assembly_id);
            }
            self.pending_samples.retain(|_, queue| !queue.is_empty());
        }
    }

    /// Decode every unique sample to device-rate mono PCM and assemble the
    /// bank (ADR-0103 §6). Pure + `Send` so it runs on the blocking-dispatch
    /// worker, off the realtime path. A failed decode aborts with a
    /// human-readable reason the cap relays as `LoadInstrumentResult::Err`.
    fn assemble_bank(
        name: String,
        regions: &[SfzRegion],
        sample_bytes: &[(String, Vec<u8>)],
        target_rate: u32,
    ) -> BankAssemblyOutput {
        // Decode each unique sample, carrying its source rate so loop frame
        // offsets can be scaled by the same resample ratio applied to the
        // PCM (ADR-0103 §6).
        let mut decoded: Vec<(String, Arc<[f32]>, u32)> = Vec::with_capacity(sample_bytes.len());
        let mut resident_bytes = 0usize;
        for (rel, bytes) in sample_bytes {
            let pcm =
                decode_wav_to_mono(bytes, target_rate).map_err(|e| format!("sample {rel}: {e}"))?;
            let source_rate = wav_source_rate(bytes).map_err(|e| format!("sample {rel}: {e}"))?;
            resident_bytes += pcm.len() * size_of::<f32>();
            decoded.push((rel.clone(), Arc::from(pcm.as_slice()), source_rate));
        }

        let mut bank_regions = Vec::with_capacity(regions.len());
        for region in regions {
            let (pcm, source_rate) = decoded
                .iter()
                .find(|(rel, _, _)| rel == &region.sample)
                .map(|(_, pcm, source_rate)| (Arc::clone(pcm), *source_rate))
                .ok_or_else(|| format!("region references unfetched sample {}", region.sample))?;
            let loop_region = region
                .loop_spec
                .and_then(|lp| scale_loop(lp, source_rate, target_rate, pcm.len()));
            bank_regions.push(SampleRegion {
                lokey: region.lokey,
                hikey: region.hikey,
                lovel: region.lovel,
                hivel: region.hivel,
                pitch_keycenter: region.pitch_keycenter,
                pcm,
                loop_region,
            });
        }

        Ok(Arc::new(SampleBank {
            name,
            regions: bank_regions,
            resident_bytes,
        }))
    }

    /// Read a WAV asset's source sample rate from its header (ADR-0103 §6).
    /// Bank assembly needs it to scale a region's loop frame offsets by the
    /// load-time resample ratio; `decode_wav_to_mono` consumes the same
    /// header but only returns the resampled PCM. Parses the header chunk
    /// only — the sample data is not read.
    fn wav_source_rate(bytes: &[u8]) -> Result<u32, String> {
        let reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let rate = reader.spec().sample_rate;
        if rate == 0 {
            return Err("zero sample rate".to_owned());
        }
        Ok(rate)
    }

    /// Scale a region's source-frame loop bounds into device-rate fractional
    /// positions (ADR-0103 §6): multiply by the resample ratio
    /// `target_rate / source_rate` — the same ratio the PCM was resampled
    /// by at load — and clamp `loop_end` to the resampled length. Returns
    /// `None` when the resampled region is too short to loop or the bounds
    /// collapse after clamping, degrading the region to unlooped.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn scale_loop(
        lp: SfzLoop,
        source_rate: u32,
        target_rate: u32,
        resampled_len: usize,
    ) -> Option<SampleLoop> {
        if resampled_len < 2 || source_rate == 0 {
            return None;
        }
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let start = f64::from(lp.start) * ratio;
        let end = (f64::from(lp.end) * ratio).min(resampled_len as f64);
        if start + 1.0 >= end {
            return None;
        }
        Some(SampleLoop {
            start: start as f32,
            end: end as f32,
        })
    }

    /// The directory portion of an fs path (everything before the last
    /// `/`), or `""` when the path has no directory. A bank's samples are
    /// addressed relative to the `.sfz`'s own directory (ADR-0103 §5).
    fn sfz_dir(path: &str) -> &str {
        match path.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        }
    }

    /// Join a sample path onto the `.sfz`'s directory. An empty directory
    /// leaves the sample as-is.
    fn join_fs(dir: &str, rel: &str) -> String {
        if dir.is_empty() {
            rel.to_owned()
        } else {
            format!("{dir}/{rel}")
        }
    }

    /// Derive a bank name from the `.sfz` filename stem (the last path
    /// segment without its extension). Falls back to `"instrument"` for a
    /// pathological empty stem.
    fn bank_name_from_path(path: &str) -> String {
        let file = path.rsplit('/').next().unwrap_or(path);
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        if stem.is_empty() {
            "instrument".to_owned()
        } else {
            stem.to_owned()
        }
    }

    /// The first instrument id available to a loaded bank — one past the
    /// last compiled-in built-in. The cap's `next_instrument_id` starts
    /// here, the synth's bank table begins at the same offset.
    fn builtin_id_ceiling() -> u8 {
        // `BUILTINS` is a small fixed table (11 today); the length fits a
        // `u8` with room to spare, and a load count that overflowed `u8`
        // would be absurd.
        #[allow(clippy::cast_possible_truncation)]
        let n = BUILTINS.len() as u8;
        n
    }

    impl Drop for AudioCapability {
        fn drop(&mut self) {
            // Drop the shutdown sender first; the worker's `recv()`
            // returns, it drops the cpal::Stream on its own thread, and
            // exits. Then we join.
            self.shutdown.take();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    #[actor]
    impl NativeActor for AudioCapability {
        type Config = AudioConfig;

        /// ADR-0039 + ADR-0074 Phase 5 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.audio";

        /// Boot the cap. Always succeeds — cpal init failure logs a
        /// warning and falls back to nop mode (per ADR-0039: audio is a
        /// peripheral, not infrastructure). The cap always claims its
        /// mailbox so agents on chassis without audio still get loud
        /// `Err` replies for `SetMasterGain` instead of timing out.
        fn init(config: AudioConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            if config.disabled {
                tracing::info!(
                    target: "aether_substrate::audio",
                    "AETHER_AUDIO_DISABLE=1 — skipping cpal init",
                );
                return Ok(Self::nop());
            }
            match spawn_audio_worker(config.requested_sample_rate) {
                Ok((sender, sample_rate, thread, shutdown)) => Ok(Self {
                    sender: Some(sender),
                    // Audio device rates are bounded well below 2^24 —
                    // exact in f32, matching the synth's own conversion.
                    #[allow(clippy::cast_precision_loss)]
                    sample_rate: Some(sample_rate as f32),
                    pending_tracks: HashMap::new(),
                    pending_instruments: HashMap::new(),
                    assemblies: HashMap::new(),
                    pending_samples: HashMap::new(),
                    next_assembly_id: 0,
                    next_instrument_id: builtin_id_ceiling(),
                    thread: Some(thread),
                    shutdown: Some(shutdown),
                }),
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::audio",
                        error = %e,
                        "audio pipeline init failed — NoteOn/NoteOff will be nop, SetMasterGain will reply Err",
                    );
                    Ok(Self::nop())
                }
            }
        }

        /// Start a note.
        ///
        /// # Agent
        /// Fire-and-forget. The synth keys voices on
        /// `(sender, instrument_id, pitch)`; sending two `NoteOn`s with
        /// the same triple is a no-op.
        #[handler]
        fn on_note_on(&self, ctx: &mut NativeCtx<'_>, mail: NoteOn) {
            let Some(s) = self.sender.as_ref() else {
                return;
            };
            let ev = AudioEvent::NoteOn {
                sender_mailbox: sender_mailbox_id(ctx.reply_target()),
                pitch: mail.pitch,
                velocity: mail.velocity,
                instrument_id: mail.instrument_id,
            };
            if s.push(ev).is_err() {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    "event queue full — dropping note_on",
                );
            }
        }

        /// Stop a note. Pairs with `on_note_on` by voice key.
        ///
        /// # Agent
        /// Fire-and-forget.
        #[handler]
        fn on_note_off(&self, ctx: &mut NativeCtx<'_>, mail: NoteOff) {
            let Some(s) = self.sender.as_ref() else {
                return;
            };
            let ev = AudioEvent::NoteOff {
                sender_mailbox: sender_mailbox_id(ctx.reply_target()),
                pitch: mail.pitch,
                instrument_id: mail.instrument_id,
            };
            if s.push(ev).is_err() {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    "event queue full — dropping note_off",
                );
            }
        }

        /// Set the master gain.
        ///
        /// # Agent
        /// Reply: `SetMasterGainResult`. `Ok { applied_gain }` clamps to
        /// `0.0..=1.0`; `Err` on chassis without audio.
        #[handler]
        fn on_set_master_gain(
            &self,
            _ctx: &mut NativeCtx<'_>,
            mail: SetMasterGain,
        ) -> SetMasterGainResult {
            let applied = mail.gain.clamp(0.0, 1.0);
            let Some(s) = self.sender.as_ref() else {
                return SetMasterGainResult::Err {
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                };
            };
            let _ = s.push(AudioEvent::SetMasterGain { gain: applied });
            tracing::info!(
                target: "aether_substrate::audio",
                requested = mail.gain,
                applied,
                "master gain set",
            );
            SetMasterGainResult::Ok {
                applied_gain: applied,
            }
        }

        /// Schedule a batch of timed note events (ADR-0104).
        ///
        /// # Agent
        /// Reply: `ScheduleResult`. Validates the batch synchronously — a
        /// non-empty size at or below `SCHEDULE_MAX_EVENTS` and every
        /// `at_millis` within the `SCHEDULE_MAX_MILLIS` horizon — and
        /// rejects the whole batch atomically with a loud `Err` on any
        /// invalid entry. On success the accepted batch crosses to the
        /// audio callback as one event and `Ok { accepted }` reports the
        /// count. Nop chassis (headless / hub / disabled / no device) reply
        /// `Err` fail-fast.
        #[handler]
        fn on_schedule(&self, ctx: &mut NativeCtx<'_>, mail: Schedule) -> ScheduleResult {
            let Some(s) = self.sender.as_ref() else {
                return ScheduleResult::Err {
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                };
            };
            if mail.events.is_empty() {
                return ScheduleResult::Err {
                    error: "schedule batch carries no events".to_owned(),
                };
            }
            if mail.events.len() > SCHEDULE_MAX_EVENTS {
                return ScheduleResult::Err {
                    error: format!(
                        "schedule batch of {} events exceeds the {SCHEDULE_MAX_EVENTS}-event cap",
                        mail.events.len(),
                    ),
                };
            }
            if let Some(over) = mail
                .events
                .iter()
                .find(|e| e.at_millis > SCHEDULE_MAX_MILLIS)
            {
                return ScheduleResult::Err {
                    error: format!(
                        "scheduled event at {} millis exceeds the {SCHEDULE_MAX_MILLIS}-millis horizon",
                        over.at_millis,
                    ),
                };
            }
            // Length is validated at or below SCHEDULE_MAX_EVENTS, which
            // fits u32, so the accepted count never truncates.
            #[allow(clippy::cast_possible_truncation)]
            let accepted = mail.events.len() as u32;
            let ev = AudioEvent::Schedule {
                sender_mailbox: sender_mailbox_id(ctx.reply_target()),
                events: mail.events,
            };
            if s.push(ev).is_err() {
                return ScheduleResult::Err {
                    error: "audio event queue full — schedule dropped".to_owned(),
                };
            }
            ScheduleResult::Ok { accepted }
        }

        /// Fetch, decode, and play an audio asset in the track lane.
        ///
        /// # Agent
        /// Reply: `PlayTrackResult`. The cap forwards an `aether.fs.read`
        /// for `namespace://path`, decodes + resamples the bytes off the
        /// realtime path, and replies `Ok` once the track has started or
        /// `Err` with the failure reason (bad path, malformed/unsupported
        /// file, or a chassis without audio). Re-playing the same
        /// `(sender, lane, namespace, path)` key restarts the track.
        #[handler::manual]
        fn on_play_track(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: PlayTrack) {
            // Nop chassis (headless / hub / disabled / no device): fail
            // fast with a loud Err (ADR-0103 §7).
            if self.sender.is_none() || self.sample_rate.is_none() {
                ctx.reply(&PlayTrackResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    lane: mail.lane,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                });
                return;
            }

            let source = ctx.reply_target();
            let sender_mailbox = sender_mailbox_id(source);
            let key = (mail.namespace.clone(), mail.path.clone());
            self.pending_tracks
                .entry(key)
                .or_default()
                .push_back(PendingTrack {
                    source,
                    sender_mailbox,
                    lane: mail.lane,
                    gain: mail.gain,
                    looping: mail.looping,
                });

            // Forward the read to the single fs resolver (ADR-0041) — the
            // reply (`ReadResult`) routes back to this cap's own mailbox,
            // where `on_read_result` correlates it. Keeping the read on the
            // fs cap means the audio cap never grows a second namespace
            // registry (ADR-0103 §2).
            let read = Read {
                namespace: mail.namespace,
                path: mail.path,
            };
            ctx.actor::<FsCapability>().send(&read);
        }

        /// Correlate a forwarded `aether.fs.read` reply (ADR-0103 §2).
        ///
        /// One handler serves three fetch paths keyed by which pending
        /// table the echoed `(namespace, path)` matches: a `play_track`
        /// track, a `load_instrument` `.sfz`, or one of a bank's sample
        /// WAVs. `Ok` routes the bytes onward (decode dispatch / parse /
        /// accumulate); `Err` relays the fs error to whichever original
        /// requester is waiting. The deferred reply lands on that caller —
        /// not the fs mailbox the read reply came from.
        #[handler::manual]
        fn on_read_result(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: ReadResult) {
            match mail {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    if let Some(pending) = self.take_pending(&namespace, &path) {
                        self.start_track_decode(ctx, &pending, namespace, path, bytes);
                    } else if let Some(pending) = self.take_pending_instrument(&namespace, &path) {
                        self.on_sfz_loaded(ctx, &pending, namespace, path, &bytes);
                    } else if let Some(assembly_id) = self.take_pending_sample(&namespace, &path) {
                        self.on_sample_loaded(ctx, assembly_id, &path, bytes);
                    }
                    // else: a stray / late reply with no parked request.
                }
                ReadResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    let reason = format!("file read failed: {error:?}");
                    if let Some(pending) = self.take_pending(&namespace, &path) {
                        ctx.reply_to(
                            pending.source,
                            &PlayTrackResult::Err {
                                namespace,
                                path,
                                lane: pending.lane,
                                error: reason,
                            },
                        );
                    } else if let Some(pending) = self.take_pending_instrument(&namespace, &path) {
                        ctx.reply_to(
                            pending.source,
                            &LoadInstrumentResult::Err {
                                namespace,
                                path,
                                error: reason,
                            },
                        );
                    } else if let Some(assembly_id) = self.take_pending_sample(&namespace, &path) {
                        self.fail_assembly(ctx, assembly_id, reason);
                    }
                }
            }
        }

        /// Decode completion (ADR-0093 §3). On success push the decoded
        /// PCM into the track lane and reply `Ok`; on a decode failure
        /// reply `Err`. Either way `resolve_with` re-replies through the
        /// captured `play_track` caller and drops the hold.
        #[handler::manual(task)]
        fn on_track_decoded(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            done: TaskDone<DecodeOutput, TrackDecodeContext>,
        ) {
            // Build the lane event while the output/context borrows are
            // live, then end them before `resolve_with` consumes `done`.
            let decode_err = match done.output() {
                Ok(pcm) => {
                    let cx = done.context();
                    if let Some(sender) = self.sender.as_ref() {
                        let event = AudioEvent::TrackStart {
                            sender_mailbox: cx.sender_mailbox,
                            lane: cx.lane.clone(),
                            namespace: cx.namespace.clone(),
                            path: cx.path.clone(),
                            pcm: Arc::from(pcm.as_slice()),
                            gain: cx.gain,
                            looping: cx.looping,
                        };
                        if sender.push(event).is_err() {
                            tracing::warn!(
                                target: "aether_substrate::audio",
                                "event queue full — dropping track_start",
                            );
                        }
                    }
                    None
                }
                Err(error) => Some(error.to_string()),
            };

            match decode_err {
                None => done.resolve_with(ctx, |_out, cx| PlayTrackResult::Ok {
                    namespace: cx.namespace.clone(),
                    path: cx.path.clone(),
                    lane: cx.lane.clone(),
                }),
                Some(error) => done.resolve_with(ctx, move |_out, cx| PlayTrackResult::Err {
                    namespace: cx.namespace.clone(),
                    path: cx.path.clone(),
                    lane: cx.lane.clone(),
                    error,
                }),
            }
        }

        /// Fade out and retire a track started by `play_track`.
        ///
        /// # Agent
        /// Fire-and-forget. Matched on `(sender, lane, namespace, path)`;
        /// stopping a track that isn't playing is a no-op.
        #[handler]
        fn on_stop_track(&mut self, ctx: &mut NativeCtx<'_>, mail: StopTrack) {
            let Some(sender) = self.sender.as_ref() else {
                return;
            };
            let event = AudioEvent::TrackStop {
                sender_mailbox: sender_mailbox_id(ctx.reply_target()),
                lane: mail.lane,
                namespace: mail.namespace,
                path: mail.path,
            };
            if sender.push(event).is_err() {
                tracing::warn!(
                    target: "aether_substrate::audio",
                    "event queue full — dropping track_stop",
                );
            }
        }

        /// Load a sampled instrument bank from an `.sfz` file (ADR-0103
        /// §4/§5).
        ///
        /// # Agent
        /// Reply: `LoadInstrumentResult`. The cap forwards an
        /// `aether.fs.read` for the `.sfz` at `namespace://path`, parses the
        /// SFZ subset, fetches every sample it references, decodes and
        /// resamples them off the realtime path, and appends the assembled
        /// bank to the registry. `Ok` carries the assigned `instrument_id`
        /// (thread it into `note_on`), the bank `name`, and `resident_bytes`;
        /// `Err` carries the failure reason (bad path, malformed `.sfz` /
        /// sample, or a chassis without audio). Loaded ids are
        /// session-scoped.
        #[handler::manual]
        fn on_load_instrument(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: LoadInstrument) {
            // Nop chassis (headless / hub / disabled / no device): fail
            // fast with a loud Err (ADR-0103 §7).
            if self.sender.is_none() || self.sample_rate.is_none() {
                ctx.reply(&LoadInstrumentResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: "audio pipeline not initialised on this desktop substrate".to_owned(),
                });
                return;
            }

            let source = ctx.reply_target();
            let key = (mail.namespace.clone(), mail.path.clone());
            self.pending_instruments
                .entry(key)
                .or_default()
                .push_back(PendingInstrument { source });

            // Forward the `.sfz` read to the single fs resolver (ADR-0041);
            // the `ReadResult` routes back to `on_read_result`, which parses
            // it and fans out the sample reads (ADR-0103 §2/§5).
            let read = Read {
                namespace: mail.namespace,
                path: mail.path,
            };
            ctx.actor::<FsCapability>().send(&read);
        }

        /// Bank-assembly completion (ADR-0093 §3 / ADR-0103 §4). On success
        /// assign the next instrument id, register the bank with the synth,
        /// and reply `Ok` with the id / name / resident bytes; on a decode
        /// failure reply `Err`. Either way `resolve_with` re-replies through
        /// the captured `load_instrument` caller and drops the hold.
        #[handler::manual(task)]
        fn on_instrument_assembled(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            done: TaskDone<BankAssemblyOutput, BankAssemblyContext>,
        ) {
            // The assembled-or-failed reply value, built while the
            // output/context borrows are live so the side effects (id
            // assignment, register event) run before `resolve_with` consumes
            // `done`.
            let outcome: LoadInstrumentResult = match done.output() {
                Ok(bank) => {
                    if let Some(sender) = self.sender.as_ref() {
                        let instrument_id = self.next_instrument_id;
                        self.next_instrument_id = self.next_instrument_id.saturating_add(1);
                        let name = bank.name.clone();
                        // PCM byte counts are bounded well below u64.
                        let resident_bytes = bank.resident_bytes as u64;
                        if sender
                            .push(AudioEvent::RegisterInstrument {
                                id: instrument_id,
                                bank: Arc::clone(bank),
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                target: "aether_substrate::audio",
                                "event queue full — dropping register_instrument",
                            );
                        }
                        tracing::info!(
                            target: "aether_substrate::audio",
                            instrument_id,
                            name = %name,
                            resident_bytes,
                            "sampled instrument loaded",
                        );
                        LoadInstrumentResult::Ok {
                            instrument_id,
                            name,
                            resident_bytes,
                        }
                    } else {
                        let cx = done.context();
                        LoadInstrumentResult::Err {
                            namespace: cx.namespace.clone(),
                            path: cx.path.clone(),
                            error: "audio pipeline not initialised on this desktop substrate"
                                .to_owned(),
                        }
                    }
                }
                Err(error) => {
                    let cx = done.context();
                    LoadInstrumentResult::Err {
                        namespace: cx.namespace.clone(),
                        path: cx.path.clone(),
                        error: error.clone(),
                    }
                }
            };
            done.resolve_with(ctx, move |_out, _cx| outcome);
        }
    }

    /// Spawn the audio worker thread that owns `cpal::Stream` for the
    /// cap's lifetime. The worker:
    ///   1. Builds the cpal pipeline on its own thread (`!Send`
    ///      constraint).
    ///   2. Sends the [`AudioEventSender`] back over the init channel.
    ///   3. Parks on the shutdown channel, holding the stream alive.
    ///   4. On shutdown sender drop, `recv()` returns and the stream
    ///      drops on this thread.
    ///
    /// Returns the producer side of the synth event queue plus the
    /// worker thread + shutdown sender for the cap to manage. On
    /// pipeline build failure, the worker thread exits cleanly and the
    /// caller sees the error.
    fn spawn_audio_worker(
        requested_sample_rate: Option<u32>,
    ) -> Result<(AudioEventSender, u32, JoinHandle<()>, mpsc::Sender<()>), AudioBuildError> {
        let (init_tx, init_rx) =
            mpsc::channel::<Result<(AudioEventSender, u32), AudioBuildError>>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        // cpal device-callback thread, owned by the audio backend — not actor work,
        // no ctx, no inbound chain; the audio peripheral runs outside the mail layer.
        #[allow(clippy::disallowed_methods)]
        let thread = thread::Builder::new()
            .name("aether-audio-cpal".into())
            .spawn(move || {
                match try_build_pipeline(requested_sample_rate) {
                    Ok(pipeline) => {
                        let _ = init_tx.send(Ok((pipeline.sender.clone(), pipeline.sample_rate)));
                        drop(init_tx);
                        let _ = shutdown_rx.recv();
                        drop(pipeline); // cpal::Stream tears down here
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| {
                AudioBuildError::StreamBuild(format!("worker thread spawn failed: {e}"))
            })?;

        match init_rx.recv() {
            Ok(Ok((sender, sample_rate))) => Ok((sender, sample_rate, thread, shutdown_tx)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(AudioBuildError::StreamBuild(
                    "audio worker closed channel before init".to_string(),
                ))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        // `sender.push(...).unwrap()` reads as test setup — the channel
        // is local and never full / closed during the test. `.expect`
        // per call would be pure noise.
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::test_chassis::{
            TestChassis, boot_test_chassis_with, decode_session_reply, drive_task_completion,
            fresh_substrate, test_mailer_and_rx,
        };
        use aether_actor::Addressable;
        use aether_data::{MailId, SessionToken, Source, SourceAddr, Uuid};
        use aether_kinds::FsError;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::chassis::error::BootError;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::registry;
        use aether_substrate::{
            EgressEvent, HubOutbound, InboxHandler, Mailer, OwnedDispatch, Registry,
        };
        use std::time::Duration;

        /// Ids 0–4 are wire-stable: reordering or re-patching them breaks
        /// every `NoteOn.instrument_id` already in the wild. This pins
        /// their names and oscillator waves so an accidental edit fails
        /// loudly. Adds (piano / `electric_piano` / pad) go at the end.
        #[test]
        fn oscillator_ids_zero_through_four_are_wire_stable() {
            let waves = [
                ("sine_lead", Wave::Sine),
                ("square_bass", Wave::Square),
                ("triangle", Wave::Triangle),
                ("saw_lead", Wave::Saw),
                ("pluck", Wave::Saw),
            ];
            for (id, (name, wave)) in waves.iter().enumerate() {
                let def = &BUILTINS[id];
                assert_eq!(def.name, *name, "id {id} name drifted");
                match def.voice {
                    VoiceDef::Oscillator { wave: w, .. } => assert!(
                        matches!(
                            (w, wave),
                            (Wave::Sine, Wave::Sine)
                                | (Wave::Square, Wave::Square)
                                | (Wave::Triangle, Wave::Triangle)
                                | (Wave::Saw, Wave::Saw)
                        ),
                        "id {id} wave drifted",
                    ),
                    VoiceDef::PartialBank(_) => panic!("id {id} must stay an oscillator patch"),
                }
            }
        }

        #[test]
        fn builtin_registry_lists_eleven_patches() {
            assert_eq!(builtin_count(), 11);
            assert_eq!(
                builtin_names(),
                vec![
                    "sine_lead",
                    "square_bass",
                    "triangle",
                    "saw_lead",
                    "pluck",
                    "piano",
                    "electric_piano",
                    "pad",
                    "kick",
                    "hat",
                    "snare",
                ],
            );
        }

        /// Every id assigned before this block (0–7) is wire-stable:
        /// `NoteOn.instrument_id` values already in the wild must keep
        /// resolving to the same patch, so pin the full prior name table.
        /// The percussion adds (kick / hat / snare) go strictly after.
        #[test]
        fn prior_ids_zero_through_seven_are_wire_stable() {
            let prior = [
                "sine_lead",
                "square_bass",
                "triangle",
                "saw_lead",
                "pluck",
                "piano",
                "electric_piano",
                "pad",
            ];
            for (id, name) in prior.iter().enumerate() {
                assert_eq!(BUILTINS[id].name, *name, "id {id} name drifted");
            }
        }

        /// Pull a `PartialBankDef` out of the registry by name for the
        /// kernel tests. Panics if the named patch is not a partial bank.
        fn partial_bank_def(name: &str) -> PartialBankDef {
            let def = BUILTINS
                .iter()
                .find(|d| d.name == name)
                .expect("named builtin exists");
            match def.voice {
                VoiceDef::PartialBank(bank) => bank,
                VoiceDef::Oscillator { .. } => panic!("{name} is not a partial-bank patch"),
            }
        }

        /// Drive a kernel until it frees itself, returning the sample
        /// count. Caps iterations so a stuck voice fails the test instead
        /// of hanging.
        fn samples_until_done(voice: &mut PartialBankVoice, sample_rate: f32) -> usize {
            let dt = 1.0 / sample_rate;
            // 30 s cap at the test rate — exact for usize.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let cap = (sample_rate * 30.0) as usize;
            let mut n = 0;
            while !voice.done() && n < cap {
                voice.next_sample(dt);
                n += 1;
            }
            assert!(voice.done(), "voice did not free itself within the cap");
            n
        }

        #[test]
        fn partial_bank_envelope_decreases_after_attack() {
            let def = partial_bank_def("piano");
            let mut voice = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
            let dt = 1.0 / 48_000.0;
            // Run past the (near-zero) attack into sustain.
            while !voice.in_sustain() {
                voice.next_sample(dt);
            }
            let mut last = voice.envelope_level();
            for _ in 0..4_000 {
                voice.next_sample(dt);
                let level = voice.envelope_level();
                assert!(
                    level <= last + f32::EPSILON,
                    "partial envelope must not rise in sustain: {level} > {last}",
                );
                last = level;
            }
        }

        #[test]
        fn higher_pitch_decays_in_fewer_samples() {
            let def = partial_bank_def("piano");
            let mut low = PartialBankVoice::new(40, 100, &def, 0.3, 48_000.0);
            let mut high = PartialBankVoice::new(84, 100, &def, 0.3, 48_000.0);
            let low_samples = samples_until_done(&mut low, 48_000.0);
            let high_samples = samples_until_done(&mut high, 48_000.0);
            assert!(
                high_samples < low_samples,
                "high pitch ({high_samples}) should ring shorter than low ({low_samples})",
            );
        }

        #[test]
        fn upper_partial_energy_rises_with_velocity() {
            let def = partial_bank_def("piano");
            let soft = PartialBankVoice::new(60, 20, &def, 0.3, 48_000.0);
            let hard = PartialBankVoice::new(60, 120, &def, 0.3, 48_000.0);
            let upper_share = |v: &PartialBankVoice| -> f32 {
                let amps = v.partial_amps();
                let upper: f32 = amps[PARTIAL_COUNT / 2..].iter().map(|a| a.abs()).sum();
                let total: f32 = amps.iter().map(|a| a.abs()).sum();
                upper / total
            };
            assert!(
                upper_share(&hard) > upper_share(&soft),
                "harder strike must shift energy toward upper partials",
            );
        }

        #[test]
        fn note_off_silences_faster_than_natural_decay() {
            let def = partial_bank_def("piano");
            let mut undamped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
            let mut damped = PartialBankVoice::new(60, 100, &def, 0.3, 48_000.0);
            let dt = 1.0 / 48_000.0;
            // Let both ring briefly, then release only the damped one.
            for _ in 0..480 {
                undamped.next_sample(dt);
                damped.next_sample(dt);
            }
            damped.note_off();
            let damped_samples = 480 + samples_until_done(&mut damped, 48_000.0);
            let undamped_samples = 480 + samples_until_done(&mut undamped, 48_000.0);
            assert!(
                damped_samples < undamped_samples,
                "note_off damper ({damped_samples}) should beat natural decay ({undamped_samples})",
            );
        }

        #[test]
        fn partial_bank_voice_frees_itself_when_silent() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            // id 5 is piano; high pitch rings out quickly.
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 96,
                    velocity: 100,
                    instrument_id: 5,
                })
                .unwrap();
            let mut buf = vec![0.0f32; 4_800];
            for _ in 0..200 {
                synth.fill(&mut buf, 1);
                if synth.voice_count() == 0 {
                    break;
                }
            }
            assert_eq!(synth.voice_count(), 0, "held piano voice never freed");
        }

        #[test]
        fn pad_holds_level_through_sustain() {
            let def = partial_bank_def("pad");
            let mut voice = PartialBankVoice::new(60, 100, &def, 0.18, 48_000.0);
            let dt = 1.0 / 48_000.0;
            // Drive through the long attack into sustain.
            while !voice.in_sustain() {
                voice.next_sample(dt);
            }
            let level = voice.envelope_level();
            for _ in 0..48_000 {
                voice.next_sample(dt);
            }
            let after = voice.envelope_level();
            assert!(
                (after - level).abs() < 1.0e-3,
                "pad must sustain its level while held: {level} -> {after}",
            );
        }

        /// A sustain-holding ADSR (instant attack, no decay, full
        /// sustain) so a kernel test reads the raw waveform without the
        /// envelope shaping the level.
        const HOLD_ADSR: Adsr = Adsr {
            attack_s: 0.0,
            decay_s: 0.0,
            sustain: 1.0,
            release_s: 0.1,
        };

        /// Build an oscillator voice and collect `n` samples at 48 kHz.
        fn collect_osc(wave: Wave, base_amp: f32, seed: u32, n: usize) -> Vec<f32> {
            let mut voice = OscVoice::new(60, 100, wave, HOLD_ADSR, base_amp, 48_000.0, seed);
            let dt = 1.0 / 48_000.0;
            (0..n).map(|_| voice.next_sample(dt)).collect()
        }

        /// Count sign changes across a sample window — a proxy for
        /// instantaneous frequency.
        fn zero_crossings(samples: &[f32]) -> usize {
            samples
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count()
        }

        #[test]
        fn noise_is_bounded_and_nonzero() {
            let samples = collect_osc(
                Wave::Noise {
                    lowpass: 1.0,
                    tone_mix: 0.0,
                },
                1.0,
                voice_seed(MailboxId(1), 9, 60),
                4_000,
            );
            assert!(
                samples.iter().all(|s| s.abs() <= 1.0 + f32::EPSILON),
                "noise sample escaped [-1, 1]",
            );
            assert!(
                samples.iter().any(|s| s.abs() > 0.0),
                "noise produced silence",
            );
        }

        #[test]
        fn noise_is_deterministic_for_a_fixed_voice_key() {
            let seed = voice_seed(MailboxId(7), 9, 64);
            let wave = Wave::Noise {
                lowpass: 0.8,
                tone_mix: 0.0,
            };
            let first = collect_osc(wave, 1.0, seed, 2_000);
            let second = collect_osc(wave, 1.0, seed, 2_000);
            assert_eq!(first, second, "fixed-key noise must be reproducible");
        }

        #[test]
        fn lowpass_reduces_sample_to_sample_delta() {
            let seed = voice_seed(MailboxId(1), 9, 60);
            let unfiltered = collect_osc(
                Wave::Noise {
                    lowpass: 1.0,
                    tone_mix: 0.0,
                },
                1.0,
                seed,
                8_000,
            );
            let filtered = collect_osc(
                Wave::Noise {
                    lowpass: 0.15,
                    tone_mix: 0.0,
                },
                1.0,
                seed,
                8_000,
            );
            let mean_delta = |s: &[f32]| -> f32 {
                let sum: f32 = s.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
                // window count is bounded and small — exact in f32.
                #[allow(clippy::cast_precision_loss)]
                let count = (s.len() - 1) as f32;
                sum / count
            };
            assert!(
                mean_delta(&filtered) < mean_delta(&unfiltered),
                "lowpassed noise should be smoother sample-to-sample",
            );
        }

        #[test]
        fn pitch_sweep_zero_crossing_rate_falls_toward_base() {
            let mut voice = OscVoice::new(60, 100, Wave::Sine, HOLD_ADSR, 1.0, 48_000.0, 1)
                .with_pitch_sweep(
                    PitchSweep {
                        start_ratio: 8.0,
                        time_constant_secs: 0.05,
                    },
                    48_000.0,
                );
            let dt = 1.0 / 48_000.0;
            let samples: Vec<f32> = (0..19_200).map(|_| voice.next_sample(dt)).collect();
            let onset = zero_crossings(&samples[0..2_400]);
            let settled = zero_crossings(&samples[16_800..19_200]);
            assert!(
                settled < onset,
                "swept pitch should slow toward the base frequency: onset {onset}, settled {settled}",
            );
        }

        // ADR-0090: the confique migration is byte-identical to the prior
        // hand-rolled reader. These exercise resolution without touching
        // process env (issue 464).

        #[test]
        fn audio_from_env_defaults_match() {
            use confique::Config as _;
            // No `.env()` source: literal defaults only — env-free.
            // The Layer field is `sample_rate` (the derive's
            // `layer_field = "sample_rate"` drops the `requested_`
            // prefix on the wire shape); the domain field stays
            // `requested_sample_rate`.
            let layer = AudioConfigLayer::builder().load().expect("defaults load");
            let default = AudioConfig::default();
            assert_eq!(layer.disabled, default.disabled);
            assert_eq!(layer.sample_rate, None);
            assert_eq!(default.requested_sample_rate, None);
        }

        #[test]
        fn note_on_off_lifecycle() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
            let mut buf = vec![0.0f32; 480];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1);
            assert!(buf.iter().any(|s| s.abs() > 0.0));

            sender
                .push(AudioEvent::NoteOff {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    instrument_id: 0,
                })
                .unwrap();
            // Compile-time constant; trivially exact for usize.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let release_samples = (0.5 * 48_000.0) as usize;
            let mut tail = vec![0.0f32; release_samples];
            synth.fill(&mut tail, 1);
            assert_eq!(synth.voice_count(), 0);
        }

        // ADR-0104 scheduled note events. These drive `fill` with known
        // block sizes against the synth's frame clock, so the frame a
        // scheduled event fires on is deterministic.

        #[test]
        fn scheduled_note_fires_at_its_exact_frame() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            // 1 ms at 48 kHz is exactly 48 frames.
            sender
                .push(AudioEvent::Schedule {
                    sender_mailbox: MailboxId(1),
                    events: vec![ScheduledEvent {
                        at_millis: 1,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                        },
                    }],
                })
                .unwrap();
            let mut buf = vec![0.0f32; 1];
            // The first drain converts the offset to due frame 48 and parks
            // it; rendering frames 0..47 must not fire it early.
            for _ in 0..48 {
                synth.fill(&mut buf, 1);
            }
            assert_eq!(
                synth.voice_count(),
                0,
                "scheduled note fired before its frame"
            );
            assert_eq!(synth.scheduled_count(), 1, "event left the heap too early");
            // The 49th fill renders absolute frame 48 — the exact due frame.
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1, "scheduled note missed its frame");
            assert_eq!(
                synth.scheduled_count(),
                0,
                "fired event not drained from the heap"
            );
            assert!(synth.has_voice_with_pitch(60));
        }

        #[test]
        fn simultaneous_scheduled_events_stay_a_chord() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            // Two notes at the same offset share one receipt timebase, so
            // they fire on the same frame — a chord stays a chord.
            sender
                .push(AudioEvent::Schedule {
                    sender_mailbox: MailboxId(1),
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
                            at_millis: 0,
                            event: ScheduledNote::On {
                                pitch: 64,
                                velocity: 100,
                                instrument_id: 0,
                            },
                        },
                    ],
                })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(
                synth.voice_count(),
                2,
                "simultaneous notes did not both fire"
            );
            assert!(synth.has_voice_with_pitch(60));
            assert!(synth.has_voice_with_pitch(64));
        }

        #[test]
        fn scheduled_note_off_releases_after_its_note_on() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            // One note held for 10 ms, then released — both events in one
            // batch. The off keys the same voice as the on (same sender +
            // instrument + pitch).
            sender
                .push(AudioEvent::Schedule {
                    sender_mailbox: MailboxId(1),
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
                            at_millis: 10,
                            event: ScheduledNote::Off {
                                pitch: 60,
                                instrument_id: 0,
                            },
                        },
                    ],
                })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            // The note-on fires on the first block; the off's due frame
            // (480 at 48 kHz) is still in the future, so the voice sounds.
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1, "scheduled note-on never sounded");
            assert!(synth.has_voice_with_pitch(60));
            // Play past the off's due frame plus the 0.5 s release: the off
            // fires after the on and the voice frees.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let tail_samples = (0.6 * TEST_RATE) as usize;
            let mut tail = vec![0.0f32; tail_samples];
            synth.fill(&mut tail, 1);
            assert_eq!(
                synth.voice_count(),
                0,
                "scheduled note-off never released the voice",
            );
        }

        #[test]
        fn schedule_offset_spans_block_boundaries() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            // 2 ms == 96 frames; with 64-frame blocks the note lands in the
            // second block, never the first.
            sender
                .push(AudioEvent::Schedule {
                    sender_mailbox: MailboxId(1),
                    events: vec![ScheduledEvent {
                        at_millis: 2,
                        event: ScheduledNote::On {
                            pitch: 72,
                            velocity: 100,
                            instrument_id: 0,
                        },
                    }],
                })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 0, "fired in the wrong block");
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1, "note never fired in its block");
        }

        #[test]
        fn retrigger_same_key_replaces_voice() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            for _ in 0..3 {
                sender
                    .push(AudioEvent::NoteOn {
                        sender_mailbox: MailboxId(1),
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                    })
                    .unwrap();
            }
            let mut buf = vec![0.0f32; 128];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1);
        }

        #[test]
        fn different_senders_get_independent_voices() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            for mailbox in 1..=3 {
                sender
                    .push(AudioEvent::NoteOn {
                        sender_mailbox: MailboxId(mailbox),
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                    })
                    .unwrap();
            }
            let mut buf = vec![0.0f32; 128];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 3);
        }

        #[test]
        fn set_master_gain_clamps_above_unity() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            sender
                .push(AudioEvent::SetMasterGain { gain: 1.5 })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert!((synth.master_gain_value() - 1.0).abs() < f32::EPSILON);

            sender
                .push(AudioEvent::SetMasterGain { gain: -0.2 })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert!(synth.master_gain_value().abs() < f32::EPSILON);
        }

        #[test]
        fn unknown_instrument_id_drops_note() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: 99,
                })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 0);
        }

        #[test]
        fn voice_steal_caps_at_max_voices() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);
            for i in 0..(MAX_VOICES as u64 + 10) {
                sender
                    .push(AudioEvent::NoteOn {
                        sender_mailbox: MailboxId(i + 1),
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                    })
                    .unwrap();
            }
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), MAX_VOICES);
        }

        /// Voice-steal must evict the oldest note (lowest seq) even after
        /// the pool has been reordered by `swap_remove` in the retrigger path.
        ///
        /// Setup: fill to `MAX_VOICES - 1` with pitches `0..(MAX_VOICES - 1)`
        /// (pitch 0 gets seq 0). Retrigger pitch 0 while below capacity so no
        /// steal fires: `swap_remove` moves the last voice to index 0, making
        /// pitch 1 (seq 1, the new oldest) sit at index 1, not index 0. Fill to
        /// `MAX_VOICES`, then push one more and assert pitch 1 was evicted
        /// rather than the arbitrary voice that ended up at index 0.
        #[test]
        fn voice_steal_evicts_oldest_note() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, 48_000.0);

            // Fill to MAX_VOICES - 1. Pitch 0 -> seq 0; pitch 1 -> seq 1.
            // Pitch 1 will become the oldest surviving voice after the retrigger.
            for pitch in 0..(MAX_VOICES - 1) {
                sender
                    .push(AudioEvent::NoteOn {
                        sender_mailbox: MailboxId(1),
                        pitch: u8::try_from(pitch).unwrap(),
                        velocity: 100,
                        instrument_id: 0,
                    })
                    .unwrap();
            }
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), MAX_VOICES - 1);

            // Retrigger pitch=0 while below capacity (no steal fires).
            // swap_remove moves the last voice to index 0; the oldest
            // surviving voice (pitch=1, seq=1) is now at index 1, not index 0.
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 0,
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), MAX_VOICES - 1);
            assert!(
                synth.has_voice_with_pitch(1),
                "pitch=1 (oldest after retrigger) must still be present",
            );

            // Fill the last slot — no steal yet.
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: u8::try_from(MAX_VOICES - 1).unwrap(),
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), MAX_VOICES);

            // One more note — steal fires. The oldest voice is pitch=1 (seq=1),
            // sitting at index 1 after the retrigger scramble. A naive remove(0)
            // would evict the wrong voice; seq-based steal must evict pitch=1.
            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 100,
                    velocity: 100,
                    instrument_id: 0,
                })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), MAX_VOICES);
            assert!(
                !synth.has_voice_with_pitch(1),
                "voice steal must evict the oldest note (pitch=1, seq=1), not an arbitrary one",
            );
        }

        /// Boot the cap against a disabled config and confirm the
        /// mailbox is registered. The dispatch path itself is exercised
        /// by the synth tests above; this validates wiring.
        #[test]
        fn capability_boots_and_registers_mailbox() {
            let (registry, mailer) = fresh_substrate();
            let chassis = boot_test_chassis_with::<AudioCapability>(
                &registry,
                &mailer,
                AudioConfig {
                    disabled: true,
                    ..AudioConfig::default()
                },
            );
            assert!(
                registry.lookup(AudioCapability::NAMESPACE).is_some(),
                "audio mailbox registered"
            );
            drop(chassis);
        }

        /// Builder rejects a duplicate claim.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(AudioCapability::NAMESPACE, registry::noop_handler());

            //noinspection DuplicatedCode
            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<AudioCapability>(AudioConfig {
                    disabled: true,
                    ..AudioConfig::default()
                })
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == AudioCapability::NAMESPACE
            ));
        }

        // ADR-0103 track lane. The synth-side tests drive `Synth` directly
        // (the same pattern as the note tests); the cap-handler tests drive
        // the `on_play_track` / `on_read_result` / `on_track_decoded` /
        // `on_stop_track` arms through a `new_for_test` binding.

        const TEST_RATE: f32 = 48_000.0;

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
            assert_eq!(
                synth.track_count(),
                1,
                "re-playing the same key must restart, not stack",
            );
        }

        /// A `TrackStart` at an explicit sender + lane over the shared
        /// `(namespace, path)` — the key components the collision fix
        /// folds together.
        fn keyed_track_start(
            sender_mailbox: MailboxId,
            lane: Option<&str>,
            pcm: Arc<[f32]>,
        ) -> AudioEvent {
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
            sender
                .push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(4_800)))
                .unwrap();
            sender
                .push(keyed_track_start(MailboxId(0), Some("b"), ramp_pcm(4_800)))
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(
                synth.track_count(),
                2,
                "distinct lanes must not alias to one track",
            );
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
            assert_eq!(
                synth.track_count(),
                1,
                "stopping one lane must not silence the other",
            );
        }

        #[test]
        fn same_sender_and_lane_replays_single_track() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            for _ in 0..3 {
                sender
                    .push(keyed_track_start(MailboxId(0), Some("a"), ramp_pcm(256)))
                    .unwrap();
            }
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(
                synth.track_count(),
                1,
                "re-playing the same (sender, lane) key must restart, not stack",
            );
        }

        fn session_sender() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
        }

        /// Build a cap with a live event queue but no cpal worker — the
        /// synth-side queue is exercised directly while the handler path
        /// runs as it would on a desktop substrate.
        fn live_cap() -> (AudioCapability, Arc<ArrayQueue<AudioEvent>>) {
            let (event_sender, queue) = new_event_channel();
            let cap = AudioCapability {
                sender: Some(event_sender),
                sample_rate: Some(TEST_RATE),
                pending_tracks: HashMap::new(),
                pending_instruments: HashMap::new(),
                assemblies: HashMap::new(),
                pending_samples: HashMap::new(),
                next_assembly_id: 0,
                next_instrument_id: builtin_id_ceiling(),
                thread: None,
                shutdown: None,
            };
            (cap, queue)
        }

        /// Substrate with a registry, settlement counter, egress rx (for
        /// `drive_task_completion`), and a registered component inbox.
        ///
        /// The inbox handler discharges the ADR-0094 obligation before
        /// forwarding so the caller can observe the `OwnedDispatch` (and
        /// call `record_finished`) without tripping the debug guard on drop.
        ///
        /// Returns `(mailer, egress_rx, caller_mailbox, reply_rx)`.
        fn settlement_substrate() -> (
            Arc<Mailer>,
            mpsc::Receiver<EgressEvent>,
            MailboxId,
            mpsc::Receiver<OwnedDispatch>,
        ) {
            let reg = Arc::new(Registry::new());
            let (outbound, egress_rx) = HubOutbound::attached_loopback();
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer = Arc::new(Mailer::new(Arc::clone(&reg), store).with_outbound(outbound));
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

        #[test]
        fn play_track_happy_path_replies_ok_and_starts_a_track() {
            let (mut cap, queue) = live_cap();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));

            let root = MailId::new(MailboxId(0xC0), 1);
            let mut ctx = NativeCtx::new_dispatching(&transport, session_sender(), root, root);
            cap.on_play_track(
                &mut ctx,
                PlayTrack {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    gain: 0.8,
                    looping: false,
                    lane: None,
                },
            );
            // The cap forwarded an fs.read and parked the request.
            assert_eq!(cap.pending_tracks.len(), 1, "request not parked");

            // Synthesize the fs reply with a real WAV asset (at half the
            // device rate, so decode also resamples).
            let wav = super::super::decode::wav_int16_mono(&ramp(512), 24_000);
            let mut read_ctx = NativeCtx::new_dispatching(&transport, session_sender(), root, root);
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    bytes: wav,
                },
            );
            // The decode worker runs off-thread and pushes the completion
            // wake; route it through the cap's #[handler(task)] arm.
            drive_task_completion(&mut cap, &transport, &rx);

            match decode_session_reply::<PlayTrackResult>(&rx) {
                PlayTrackResult::Ok {
                    namespace,
                    path,
                    lane,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "track.wav");
                    assert_eq!(lane, None);
                }
                PlayTrackResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
            }
            assert!(cap.pending_tracks.is_empty(), "pending entry never cleared");
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
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));

            let mut ctx = NativeCtx::new_dispatching(
                &transport,
                session_sender(),
                MailId::NONE,
                MailId::NONE,
            );
            cap.on_play_track(
                &mut ctx,
                PlayTrack {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    gain: 1.0,
                    looping: false,
                    lane: Some("bgm".to_owned()),
                },
            );
            let wav = super::super::decode::wav_int16_mono(&ramp(512), 24_000);
            let mut read_ctx = NativeCtx::new_dispatching(
                &transport,
                session_sender(),
                MailId::NONE,
                MailId::NONE,
            );
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    bytes: wav,
                },
            );
            drive_task_completion(&mut cap, &transport, &rx);

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
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));

            let mut ctx = NativeCtx::new_dispatching(
                &transport,
                session_sender(),
                MailId::NONE,
                MailId::NONE,
            );
            cap.on_play_track(
                &mut ctx,
                PlayTrack {
                    namespace: "assets".to_owned(),
                    path: "missing.wav".to_owned(),
                    gain: 1.0,
                    looping: false,
                    lane: None,
                },
            );
            cap.on_read_result(
                &mut ctx,
                ReadResult::Err {
                    namespace: "assets".to_owned(),
                    path: "missing.wav".to_owned(),
                    error: FsError::NotFound,
                },
            );

            match decode_session_reply::<PlayTrackResult>(&rx) {
                PlayTrackResult::Err { path, error, .. } => {
                    assert_eq!(path, "missing.wav");
                    assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
                }
                PlayTrackResult::Ok { .. } => panic!("expected Err for a missing file"),
            }
            assert!(cap.pending_tracks.is_empty(), "pending entry never cleared");
            assert!(
                queue.pop().is_none(),
                "a failed read must not start a track"
            );
        }

        #[test]
        fn play_track_on_nop_chassis_replies_err() {
            let mut cap = AudioCapability::nop();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = NativeCtx::new_dispatching(
                &transport,
                session_sender(),
                MailId::NONE,
                MailId::NONE,
            );
            cap.on_play_track(
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
            assert!(
                cap.pending_tracks.is_empty(),
                "nop chassis must not park a read"
            );
            // stop_track on a nop chassis is a silent no-op (no panic).
            cap.on_stop_track(
                ctx.as_single(),
                StopTrack {
                    namespace: "assets".to_owned(),
                    path: "track.wav".to_owned(),
                    lane: None,
                },
            );
        }

        // ADR-0104 schedule handler. The cap validates the batch
        // synchronously and replies `ScheduleResult` in-handler, then
        // pushes one `Schedule` event for the accepted batch. The
        // `load_ctx` helper below builds the session-addressed context.

        #[test]
        fn schedule_happy_path_replies_ok_and_queues_one_event() {
            let (cap, queue) = live_cap();
            let (mailer, _rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = load_ctx(&transport);
            let result = cap.on_schedule(
                &mut ctx,
                Schedule {
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
                            at_millis: 500,
                            event: ScheduledNote::Off {
                                pitch: 60,
                                instrument_id: 0,
                            },
                        },
                    ],
                },
            );
            match result {
                ScheduleResult::Ok { accepted } => assert_eq!(accepted, 2),
                ScheduleResult::Err { error } => panic!("expected Ok, got Err({error})"),
            }
            // The whole batch crosses the queue as exactly one event.
            let event = queue.pop().expect("a schedule event was queued");
            match event {
                AudioEvent::Schedule { events, .. } => assert_eq!(events.len(), 2),
                other => panic!("expected Schedule, got {other:?}"),
            }
            assert!(queue.pop().is_none(), "batch must use a single queue slot");
        }

        #[test]
        fn schedule_empty_batch_replies_err() {
            let (cap, queue) = live_cap();
            let (mailer, _rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = load_ctx(&transport);
            let result = cap.on_schedule(&mut ctx, Schedule { events: vec![] });
            match result {
                ScheduleResult::Err { .. } => {}
                ScheduleResult::Ok { .. } => panic!("empty batch must reject"),
            }
            assert!(
                queue.pop().is_none(),
                "rejected batch must not queue an event"
            );
        }

        #[test]
        fn schedule_over_event_cap_rejects_atomically() {
            let (cap, queue) = live_cap();
            let (mailer, _rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = load_ctx(&transport);
            let events = (0..=SCHEDULE_MAX_EVENTS)
                .map(|_| ScheduledEvent {
                    at_millis: 0,
                    event: ScheduledNote::On {
                        pitch: 60,
                        velocity: 100,
                        instrument_id: 0,
                    },
                })
                .collect();
            let result = cap.on_schedule(&mut ctx, Schedule { events });
            match result {
                ScheduleResult::Err { error } => assert!(error.contains("cap"), "reason: {error}"),
                ScheduleResult::Ok { .. } => panic!("over-cap batch must reject"),
            }
            assert!(
                queue.pop().is_none(),
                "over-cap batch must not queue an event"
            );
        }

        #[test]
        fn schedule_over_horizon_rejects_atomically() {
            let (cap, queue) = live_cap();
            let (mailer, _rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = load_ctx(&transport);
            let result = cap.on_schedule(
                &mut ctx,
                Schedule {
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
                            at_millis: SCHEDULE_MAX_MILLIS + 1,
                            event: ScheduledNote::On {
                                pitch: 64,
                                velocity: 100,
                                instrument_id: 0,
                            },
                        },
                    ],
                },
            );
            match result {
                ScheduleResult::Err { error } => {
                    assert!(error.contains("horizon"), "reason: {error}");
                }
                ScheduleResult::Ok { .. } => panic!("over-horizon batch must reject"),
            }
            // A single bad event rejects the whole batch — the valid event
            // before it never queues.
            assert!(
                queue.pop().is_none(),
                "over-horizon batch must reject atomically"
            );
        }

        #[test]
        fn schedule_on_nop_chassis_replies_err() {
            let cap = AudioCapability::nop();
            let (mailer, _rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = load_ctx(&transport);
            let result = cap.on_schedule(
                &mut ctx,
                Schedule {
                    events: vec![ScheduledEvent {
                        at_millis: 0,
                        event: ScheduledNote::On {
                            pitch: 60,
                            velocity: 100,
                            instrument_id: 0,
                        },
                    }],
                },
            );
            match result {
                ScheduleResult::Err { .. } => {}
                ScheduleResult::Ok { .. } => panic!("nop chassis must reply Err"),
            }
        }

        /// Mono ramp samples for an in-memory WAV fixture.
        fn ramp(len: usize) -> Vec<f32> {
            #[allow(clippy::cast_precision_loss)]
            (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect()
        }

        // ADR-0103 sampled instrument banks (#1679). The synth-side tests
        // drive `Synth` directly (registry + sample-voice kernel); the
        // cap-handler tests drive `on_load_instrument` / `on_read_result` /
        // `on_instrument_assembled` through a `new_for_test` binding, the
        // same pattern as the track tests above.

        fn test_region(
            lokey: u8,
            hikey: u8,
            lovel: u8,
            hivel: u8,
            pitch_keycenter: u8,
            pcm: Vec<f32>,
        ) -> SampleRegion {
            SampleRegion {
                lokey,
                hikey,
                lovel,
                hivel,
                pitch_keycenter,
                pcm: Arc::from(pcm),
                loop_region: None,
            }
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
            Arc::new(SampleBank {
                name: "test".to_owned(),
                regions,
                resident_bytes,
            })
        }

        #[test]
        fn loaded_bank_registers_past_builtins_and_plays() {
            let (sender, queue) = new_event_channel();
            let mut synth = Synth::new(queue, TEST_RATE);
            let id = builtin_id_ceiling();
            sender
                .push(AudioEvent::RegisterInstrument {
                    id,
                    bank: test_bank(vec![test_region(0, 127, 0, 127, 60, ramp(256))]),
                })
                .unwrap();
            let mut buf = vec![0.0f32; 64];
            synth.fill(&mut buf, 1);
            assert_eq!(
                synth.bank_count(),
                1,
                "bank not appended past the built-ins"
            );

            sender
                .push(AudioEvent::NoteOn {
                    sender_mailbox: MailboxId(1),
                    pitch: 60,
                    velocity: 100,
                    instrument_id: id,
                })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 1, "loaded id did not sound a voice");
            assert!(
                buf.iter().any(|s| s.abs() > 0.0),
                "sampled instrument produced silence",
            );
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
            assert!(
                synth.bank_for(first).unwrap().select(60, 100).is_some(),
                "id {first} should resolve the first bank",
            );
            assert!(
                synth.bank_for(second).unwrap().select(72, 100).is_some(),
                "id {second} should resolve the second bank",
            );
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
                })
                .unwrap();
            synth.fill(&mut buf, 1);
            assert_eq!(synth.voice_count(), 0, "note in an uncovered gap must drop");
        }

        #[test]
        fn region_selected_by_pitch_and_velocity() {
            let bank = test_bank(vec![
                test_region(60, 71, 0, 63, 60, ramp(8)),
                test_region(60, 71, 64, 127, 60, ramp(8)),
            ]);
            let soft = bank
                .select(64, 30)
                .expect("soft region covers low velocity");
            let loud = bank
                .select(64, 110)
                .expect("loud region covers high velocity");
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
            assert!(
                (479..=481).contains(&n),
                "ended at {n} samples, expected ~480",
            );
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
            assert!(
                n < 10_000,
                "release ({n}) should end well before the sample exhausts",
            );
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
            assert!(
                !voice.done(),
                "held looped voice ended at sample exhaustion"
            );
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
            assert!(
                n < 10_000,
                "release ({n}) should retire the voice within the ramp",
            );
        }

        #[test]
        fn loop_seam_produces_no_discontinuity() {
            // A sine whose loop span is an exact multiple of its period
            // wraps phase-continuously: the per-sample output delta across
            // the seam stays in the band of an ordinary sine step, never the
            // near-full-amplitude jump a naive (non-interpolating) wrap would
            // inject (ADR-0103 §6).
            const PERIOD: usize = 64;
            #[allow(clippy::cast_precision_loss)]
            let pcm: Vec<f32> = (0..512)
                .map(|i| 0.5 * (TAU * i as f32 / PERIOD as f32).sin())
                .collect();
            // Loop span 256 == 4 * PERIOD, aligned to the period grid.
            let region = looped_region(pcm, 128.0, 384.0);
            let mut voice = SampleVoice::new(60, 100, &region);
            let dt = 1.0 / TEST_RATE;
            let mut prev = voice.next_sample(dt);
            let mut max_delta = 0.0f32;
            // Skip the attack ramp; sample well into the looped region across
            // several wraps.
            for n in 0..3000 {
                let s = voice.next_sample(dt);
                if n > 300 {
                    max_delta = max_delta.max((s - prev).abs());
                }
                prev = s;
            }
            // An ordinary sine step at this amplitude is ~0.024; a discarded
            // seam would jump by up to ~0.47. 0.05 cleanly separates them.
            assert!(
                max_delta < 0.05,
                "loop seam introduced a discontinuity (max delta {max_delta})",
            );
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
                loop_spec: Some(SfzLoop {
                    start: 100,
                    end: 400,
                    mode: super::super::sfz::LoopMode::Continuous,
                }),
            };
            let wav = super::super::decode::wav_int16_mono(&ramp(1000), 24_000);
            let bank = assemble_bank(
                "test".to_owned(),
                &[region],
                &[("a.wav".to_owned(), wav)],
                48_000,
            )
            .expect("bank assembles");
            let lp = bank.regions[0]
                .loop_region
                .expect("loop scaled through to the region");
            assert!(
                (lp.start - 200.0).abs() < 2.0,
                "loop_start should scale ~2x to 200, got {}",
                lp.start,
            );
            assert!(
                (lp.end - 800.0).abs() < 2.0,
                "loop_end should scale ~2x to 800, got {}",
                lp.end,
            );
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
                loop_spec: Some(SfzLoop {
                    start: 10,
                    end: 100_000,
                    mode: super::super::sfz::LoopMode::Continuous,
                }),
            };
            let wav = super::super::decode::wav_int16_mono(&ramp(1000), 24_000);
            let bank = assemble_bank(
                "test".to_owned(),
                &[region],
                &[("a.wav".to_owned(), wav)],
                48_000,
            )
            .expect("bank assembles");
            let region = &bank.regions[0];
            let lp = region.loop_region.expect("loop scaled through");
            #[allow(clippy::cast_precision_loss)]
            let len = region.pcm.len() as f32;
            assert!(
                lp.end <= len,
                "loop_end {} must clamp to the resampled length {len}",
                lp.end,
            );
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
            let wav = super::super::decode::wav_int16_mono(&ramp(256), 24_000);
            let bank = assemble_bank(
                "test".to_owned(),
                &[region],
                &[("a.wav".to_owned(), wav)],
                48_000,
            )
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
            for i in 0..(MAX_VOICES as u64 + 8) {
                sender
                    .push(AudioEvent::NoteOn {
                        sender_mailbox: MailboxId(i + 1),
                        pitch: 60,
                        velocity: 100,
                        instrument_id: builtin_id_ceiling(),
                    })
                    .unwrap();
            }
            synth.fill(&mut buf, 1);
            assert_eq!(
                synth.voice_count(),
                MAX_VOICES,
                "sample voices must count against MAX_VOICES and steal",
            );
        }

        fn load_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
            NativeCtx::new(transport, session_sender(), MailId::NONE, MailId::NONE)
        }

        /// ADR-0112: a `Manual` ctx for directly calling `#[handler::manual]`
        /// methods (`on_load_instrument`, `on_read_result`). Mirrors `load_ctx`
        /// but uses `new_dispatching` so the method's `OutboundReply` surface
        /// is available.
        fn manual_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_, Manual> {
            NativeCtx::new_dispatching(transport, session_sender(), MailId::NONE, MailId::NONE)
        }

        #[test]
        fn load_instrument_happy_path_replies_ok_and_registers() {
            let (mut cap, queue) = live_cap();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));

            let mut ctx = manual_ctx(&transport);
            cap.on_load_instrument(
                &mut ctx,
                LoadInstrument {
                    namespace: "assets".to_owned(),
                    path: "piano/bank.sfz".to_owned(),
                },
            );
            assert_eq!(cap.pending_instruments.len(), 1, "sfz read not parked");

            // The .sfz parses into two regions referencing two samples.
            let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
";
            let mut read_ctx = manual_ctx(&transport);
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/bank.sfz".to_owned(),
                    bytes: sfz.as_bytes().to_vec(),
                },
            );
            assert_eq!(cap.assemblies.len(), 1, "assembly not parked");
            assert_eq!(
                cap.pending_samples.len(),
                2,
                "both sample reads not fanned out"
            );

            // Half the device rate, so decode also resamples each sample.
            let wav = super::super::decode::wav_int16_mono(&ramp(256), 24_000);
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/c4.wav".to_owned(),
                    bytes: wav.clone(),
                },
            );
            // One sample still missing — no dispatch yet.
            assert_eq!(cap.assemblies.len(), 1, "assembly dispatched too early");
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "piano/c5.wav".to_owned(),
                    bytes: wav,
                },
            );
            // The last sample triggers the assembly dispatch off-thread.
            drive_task_completion(&mut cap, &transport, &rx);

            match decode_session_reply::<LoadInstrumentResult>(&rx) {
                LoadInstrumentResult::Ok {
                    instrument_id,
                    name,
                    resident_bytes,
                } => {
                    assert_eq!(instrument_id, builtin_id_ceiling());
                    assert_eq!(name, "bank");
                    assert!(resident_bytes > 0, "resident bytes not reported");
                }
                LoadInstrumentResult::Err { error, .. } => panic!("expected Ok, got Err({error})"),
            }
            assert!(cap.assemblies.is_empty(), "assembly never cleared");
            assert!(
                cap.pending_samples.is_empty(),
                "sample pending never cleared"
            );
            let event = queue.pop().expect("a register-instrument event was queued");
            assert!(
                matches!(event, AudioEvent::RegisterInstrument { id, .. } if id == builtin_id_ceiling()),
                "expected RegisterInstrument, got {event:?}",
            );
        }

        #[test]
        fn load_instrument_missing_sample_replies_err() {
            let (mut cap, queue) = live_cap();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = manual_ctx(&transport);
            cap.on_load_instrument(
                &mut ctx,
                LoadInstrument {
                    namespace: "assets".to_owned(),
                    path: "bank.sfz".to_owned(),
                },
            );
            cap.on_read_result(
                &mut ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "bank.sfz".to_owned(),
                    bytes: b"<region>\nsample=c4.wav\n".to_vec(),
                },
            );
            // The bank's only sample fails to read — the whole load fails.
            cap.on_read_result(
                &mut ctx,
                ReadResult::Err {
                    namespace: "assets".to_owned(),
                    path: "c4.wav".to_owned(),
                    error: FsError::NotFound,
                },
            );
            match decode_session_reply::<LoadInstrumentResult>(&rx) {
                LoadInstrumentResult::Err { error, .. } => {
                    assert!(error.contains("NotFound"), "fs error not surfaced: {error}");
                }
                LoadInstrumentResult::Ok { .. } => panic!("expected Err for a missing sample"),
            }
            assert!(cap.assemblies.is_empty(), "assembly never discarded");
            assert!(
                cap.pending_samples.is_empty(),
                "sample pending never cleared"
            );
            assert!(queue.pop().is_none(), "a failed bank must not register");
        }

        #[test]
        fn load_instrument_malformed_sfz_replies_err() {
            let (mut cap, queue) = live_cap();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = manual_ctx(&transport);
            cap.on_load_instrument(
                &mut ctx,
                LoadInstrument {
                    namespace: "assets".to_owned(),
                    path: "bank.sfz".to_owned(),
                },
            );
            // A control block with no regions: the parser rejects it.
            cap.on_read_result(
                &mut ctx,
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
            let mut cap = AudioCapability::nop();
            let (mailer, rx) = test_mailer_and_rx();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let mut ctx = manual_ctx(&transport);
            cap.on_load_instrument(
                &mut ctx,
                LoadInstrument {
                    namespace: "assets".to_owned(),
                    path: "bank.sfz".to_owned(),
                },
            );
            match decode_session_reply::<LoadInstrumentResult>(&rx) {
                LoadInstrumentResult::Err { .. } => {}
                LoadInstrumentResult::Ok { .. } => panic!("nop chassis must reply Err"),
            }
            assert!(
                cap.pending_instruments.is_empty(),
                "nop chassis must not park a read",
            );
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
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let (mut cap, _queue) = live_cap();
            let root = MailId::new(MailboxId(0xC0), 1);
            let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 1);

            {
                let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_play_track(
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

            let wav = super::super::decode::wav_int16_mono(&ramp(512), 24_000);
            {
                let mut read_ctx =
                    NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "track.wav".to_owned(),
                        bytes: wav,
                    },
                );
            }

            drive_task_completion(&mut cap, &transport, &rx);

            // The settlement hold was released inside resolve_with, but the
            // reply is now in-flight on the caller root — live_roots must
            // stay at 1. Pre-fix: root was MailId::NONE so record_sent_inflight
            // was a no-op and live_roots dropped to 0 here (premature settle).
            assert_eq!(
                counter.live_roots(),
                1,
                "deferred reply holds the caller chain open after hold releases",
            );

            let dispatch = reply_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("reply reached the caller inbox");
            assert_eq!(dispatch.root, root, "reply inherits the caller's root");
            mailer.record_finished(dispatch.mail_id, dispatch.root);
            assert_eq!(
                counter.live_roots(),
                0,
                "chain settles after the reply's Finished fires",
            );
        }

        /// #1693 / #1701 regression: a decode failure takes the same
        /// deferred reply path; the `Err` reply must also keep the chain
        /// live until `Finished`.
        #[test]
        fn play_track_decode_failure_settles_caller_chain() {
            let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
            let counter = Arc::clone(mailer.trace_handle().settlement_counter());
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let (mut cap, _queue) = live_cap();
            let root = MailId::new(MailboxId(0xC0), 2);
            let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 2);

            {
                let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_play_track(
                    &mut ctx,
                    PlayTrack {
                        namespace: "assets".to_owned(),
                        path: "bad.wav".to_owned(),
                        gain: 0.8,
                        looping: false,
                        lane: None,
                    },
                );
            }

            {
                let mut read_ctx =
                    NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "bad.wav".to_owned(),
                        bytes: b"not a valid wav file".to_vec(),
                    },
                );
            }

            drive_task_completion(&mut cap, &transport, &rx);

            assert_eq!(
                counter.live_roots(),
                1,
                "decode-error reply holds the caller chain open",
            );

            let dispatch = reply_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("error reply reached the caller inbox");
            assert_eq!(
                dispatch.root, root,
                "error reply inherits the caller's root"
            );
            mailer.record_finished(dispatch.mail_id, dispatch.root);
            assert_eq!(
                counter.live_roots(),
                0,
                "chain settles after the error reply's Finished fires",
            );
        }

        /// #1693 / #1701 regression: `load_instrument`'s deferred assembly
        /// reply (sfz.read → sample reads → assembly dispatch → resolve)
        /// must keep the chain UNSETTLED until the reply's `Finished` fires.
        #[test]
        fn load_instrument_deferred_reply_settles_caller_chain() {
            let (mailer, rx, caller_mailbox, reply_rx) = settlement_substrate();
            let counter = Arc::clone(mailer.trace_handle().settlement_counter());
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let (mut cap, _queue) = live_cap();
            let root = MailId::new(MailboxId(0xC0), 3);
            let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 3);

            {
                let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_load_instrument(
                    &mut ctx,
                    LoadInstrument {
                        namespace: "assets".to_owned(),
                        path: "piano/bank.sfz".to_owned(),
                    },
                );
            }

            let sfz = "\
<region>
sample=c4.wav lokey=60 hikey=71 pitch_keycenter=60
<region>
sample=c5.wav lokey=72 hikey=83 pitch_keycenter=72
";
            let wav = super::super::decode::wav_int16_mono(&ramp(256), 24_000);
            {
                let mut read_ctx =
                    NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "piano/bank.sfz".to_owned(),
                        bytes: sfz.as_bytes().to_vec(),
                    },
                );
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "piano/c4.wav".to_owned(),
                        bytes: wav.clone(),
                    },
                );
                // Last sample — triggers assembly dispatch and hold acquisition.
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "piano/c5.wav".to_owned(),
                        bytes: wav,
                    },
                );
            }

            drive_task_completion(&mut cap, &transport, &rx);

            assert_eq!(
                counter.live_roots(),
                1,
                "assembly reply holds the caller chain open after hold releases",
            );

            let dispatch = reply_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("reply reached the caller inbox");
            assert_eq!(
                dispatch.root, root,
                "assembly reply inherits the caller's root"
            );
            mailer.record_finished(dispatch.mail_id, dispatch.root);
            assert_eq!(
                counter.live_roots(),
                0,
                "chain settles after the reply's Finished fires",
            );
        }

        /// #1693 / #1701 regression: a synchronous sfz-parse failure
        /// issues a direct `reply_to` — the `Err` reply must carry the
        /// caller's root and keep the chain UNSETTLED until `Finished`.
        #[test]
        fn load_instrument_sfz_parse_failure_settles_caller_chain() {
            let (mailer, _rx, caller_mailbox, reply_rx) = settlement_substrate();
            let counter = Arc::clone(mailer.trace_handle().settlement_counter());
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0),
            ));
            let (mut cap, _queue) = live_cap();
            let root = MailId::new(MailboxId(0xC0), 4);
            let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 4);

            {
                let mut ctx = NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_load_instrument(
                    &mut ctx,
                    LoadInstrument {
                        namespace: "assets".to_owned(),
                        path: "bank.sfz".to_owned(),
                    },
                );
            }

            {
                // A <control> block with no regions fails to parse.
                let mut read_ctx =
                    NativeCtx::new_dispatching(&transport, caller_source, root, root);
                cap.on_read_result(
                    &mut read_ctx,
                    ReadResult::Ok {
                        namespace: "assets".to_owned(),
                        path: "bank.sfz".to_owned(),
                        bytes: b"<control>\ndefault_path=x/\n".to_vec(),
                    },
                );
            }

            // The parse error reply is sent synchronously — the chain is live.
            assert_eq!(
                counter.live_roots(),
                1,
                "parse-error reply holds the caller chain open",
            );

            let dispatch = reply_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("error reply reached the caller inbox");
            assert_eq!(
                dispatch.root, root,
                "parse-error reply inherits the caller's root",
            );
            mailer.record_finished(dispatch.mail_id, dispatch.root);
            assert_eq!(
                counter.live_roots(),
                0,
                "chain settles after the error reply's Finished fires",
            );
        }
    }
}
