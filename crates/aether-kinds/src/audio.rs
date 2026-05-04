//! ADR-0075 chassis-cap facade for the `aether.audio` mailbox (issue
//! 533 PR D4). The cap and its [`AudioBackend`] trait live here so
//! wasm senders can address the cap by type
//! (`ctx.send::<AudioCapability>(&note_on)`) without pulling in
//! substrate-only types — the concrete backend (cpal stream, voice
//! synth, mixer state) lives in `aether-substrate` and impls
//! [`AudioBackend`] there.
//!
//! All three handlers take `sender`. `NoteOn` / `NoteOff` use it to
//! derive the synth's voice key (one voice per
//! `(sender_mailbox, instrument_id, pitch)` triple, per ADR-0039);
//! `SetMasterGain` uses it to route the paired `SetMasterGainResult`
//! reply.

use crate::{NoteOff, NoteOn, SetMasterGain};
use aether_data::{Actor, ReplyTo};

/// Substrate-side surface a chassis installs at boot. `Send + 'static`
/// so the dispatcher thread can own the cap.
///
/// On a no-audio chassis (hub, headless, or `AETHER_AUDIO_DISABLE=1`)
/// the chassis still installs a backend — `NoteOn` / `NoteOff` no-op
/// quietly, `SetMasterGain` replies `Err` so agents fail fast rather
/// than hang waiting for a result.
pub trait AudioBackend: Send + 'static {
    /// Start a note. The voice key is
    /// `(sender_mailbox, instrument_id, pitch)` — `sender` carries
    /// the mailbox id used for that.
    fn on_note_on(&mut self, sender: ReplyTo, mail: NoteOn);

    /// Stop a note. Same voice-key shape as `on_note_on`.
    fn on_note_off(&mut self, sender: ReplyTo, mail: NoteOff);

    /// Set the master gain (clamped 0.0..=1.0). Reply
    /// `SetMasterGainResult::Ok { applied_gain }` on a real audio
    /// pipeline, `Err` on chassis without audio.
    fn on_set_master_gain(&mut self, sender: ReplyTo, mail: SetMasterGain);
}

/// Default backend used for sender-side type resolution. Senders
/// write `AudioCapability` (defaulting to [`ErasedAudioBackend`]);
/// the chassis installs a concrete `AudioBackend` impl at boot.
pub struct ErasedAudioBackend;

impl AudioBackend for ErasedAudioBackend {
    fn on_note_on(&mut self, _sender: ReplyTo, _mail: NoteOn) {
        unreachable!("ErasedAudioBackend used at runtime — chassis must install a real backend")
    }
    fn on_note_off(&mut self, _sender: ReplyTo, _mail: NoteOff) {
        unreachable!("ErasedAudioBackend used at runtime — chassis must install a real backend")
    }
    fn on_set_master_gain(&mut self, _sender: ReplyTo, _mail: SetMasterGain) {
        unreachable!("ErasedAudioBackend used at runtime — chassis must install a real backend")
    }
}

/// `aether.audio` mailbox cap.
pub struct AudioCapability<B: AudioBackend = ErasedAudioBackend> {
    backend: B,
}

impl<B: AudioBackend> AudioCapability<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: AudioBackend> Actor for AudioCapability<B> {
    /// ADR-0039 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.audio";
}

impl<B: AudioBackend> aether_data::Singleton for AudioCapability<B> {}

#[aether_data::actor]
impl<B: AudioBackend> AudioCapability<B> {
    /// Start a note.
    ///
    /// # Agent
    /// Fire-and-forget. The synth keys voices on
    /// `(sender, instrument_id, pitch)`; sending two `NoteOn`s with
    /// the same triple is a no-op.
    #[aether_data::handler]
    fn on_note_on(&mut self, sender: ReplyTo, mail: NoteOn) {
        self.backend.on_note_on(sender, mail);
    }

    /// Stop a note. Pairs with `on_note_on` by voice key.
    ///
    /// # Agent
    /// Fire-and-forget.
    #[aether_data::handler]
    fn on_note_off(&mut self, sender: ReplyTo, mail: NoteOff) {
        self.backend.on_note_off(sender, mail);
    }

    /// Set the master gain.
    ///
    /// # Agent
    /// Reply: `SetMasterGainResult`. `Ok { applied_gain }` clamps to
    /// `0.0..=1.0`; `Err` on chassis without audio.
    #[aether_data::handler]
    fn on_set_master_gain(&mut self, sender: ReplyTo, mail: SetMasterGain) {
        self.backend.on_set_master_gain(sender, mail);
    }
}
