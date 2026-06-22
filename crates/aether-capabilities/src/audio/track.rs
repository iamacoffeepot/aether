//! The track mixer lane (ADR-0103). The dedicated-lane track voice and its
//! fade state, plus the off-realtime decode hand-off the `play_track` loader
//! threads to the cap.

use std::sync::Arc;

use aether_data::{MailboxId, Source};

use super::decode::DecodeError;

/// Linear fade-out duration (seconds) applied when a track is stopped,
/// so `stop_track` releases through a short ramp instead of truncating
/// to a click (ADR-0103 §3).
pub const TRACK_FADE_SECS: f32 = 0.005;

/// Fade state of a [`TrackVoice`]. A track plays at full level until
/// `stop_track` arms a short linear fade-out; `remaining` counts down
/// per output sample and the track retires when it hits zero (ADR-0103
/// §3).
#[derive(Clone, Debug)]
pub enum TrackFade {
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
pub struct TrackVoice {
    pub sender_mailbox: MailboxId,
    pub lane: Option<String>,
    pub namespace: String,
    pub path: String,
    pub pcm: Arc<[f32]>,
    pub position: usize,
    pub gain: f32,
    pub looping: bool,
    pub fade: TrackFade,
    pub done: bool,
}

impl TrackVoice {
    pub fn new(
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
    pub fn matches(
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
    pub fn stop(&mut self, fade_samples: u32) {
        if matches!(self.fade, TrackFade::Playing) {
            let total = fade_samples.max(1);
            self.fade = TrackFade::FadingOut {
                remaining: total,
                total,
            };
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// Render this track's next sample (already gained + faded) and
    /// advance the position. Returns `0.0` once retired; an empty PCM
    /// buffer retires immediately.
    pub fn next_sample(&mut self) -> f32 {
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
pub struct PendingTrack {
    /// The original `play_track` requester — the `PlayTrackResult`
    /// reply routes here across the fs round-trip + decode.
    pub source: Source,
    /// The synth-side track key's sender component, baked into the
    /// `TrackStart` event so the lane keys by `(sender, lane,
    /// namespace, path)` while the fs correlation keys by
    /// `(namespace, path)`.
    pub sender_mailbox: MailboxId,
    /// The caller-supplied lane that disambiguates senders sharing a
    /// source mailbox; part of the synth-side track key.
    pub lane: Option<String>,
    pub gain: f32,
    pub looping: bool,
}

/// Completion context the `play_track` decode dispatch carries so the
/// `#[handler(task)]` arm can build the `TrackStart` event + the reply
/// without re-deriving anything (ADR-0093 §5). The worker produces the
/// decoded PCM; this carries the synth key + play parameters alongside.
pub struct TrackDecodeContext {
    pub sender_mailbox: MailboxId,
    pub lane: Option<String>,
    pub namespace: String,
    pub path: String,
    pub gain: f32,
    pub looping: bool,
}

/// Output of the decode dispatch worker — the resampled mono PCM, or
/// the decode failure to relay as `PlayTrackResult::Err`.
pub type DecodeOutput = Result<Vec<f32>, DecodeError>;
