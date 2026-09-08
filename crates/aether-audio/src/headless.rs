//! [`HeadlessAudioCapability`] **identity** (ADR-0122 identity/runtime
//! split): the fail-fast companion for chassis without an audio device.
//! Always-on like the primary ZST in the crate root, so a marker-only build
//! sees both identities; the runtime half is the nested `runtime::headless`
//! module, covered by the one `mod runtime;` gate.

use aether_actor::actor;

// The handler-argument and reply kinds the emitted `HandlesKind` markers lift
// verbatim from the runtime module's signatures must resolve at this file's
// root.
use crate::kinds::{
    LoadInstrument, NoteOff, NoteOn, PlayTrack, Schedule, ScheduleResult, SetMasterGain, SetMasterGainResult,
    SetReverbSend, SetReverbSendResult, SetSenderGain, SetSenderGainResult, StopTrack,
};

/// `HeadlessAudioCapability` **identity** (ADR-0122 identity/runtime split).
/// The chassis-without-audio-device companion to [`crate::AudioCapability`],
/// claiming the same `aether.audio` mailbox so desktop-designed components
/// loaded on headless keep a known recipient — the real-time trigger kinds
/// (`NoteOn` / `NoteOff` / `StopTrack`) are absorbed, and every kind that
/// promises a reply answers `Err` so a caller fails fast instead of reading
/// silence as success.
///
/// A ZST carrying only the addressing; the stateless runtime
/// (`HeadlessAudioCapabilityState`) lives behind the default `runtime` gate in
/// `runtime::headless` and names neither cpal nor the synth pipeline.
///
/// A chassis composes one of [`Self`] / [`crate::AudioCapability`], never both
/// — the chassis builder rejects double-claiming a mailbox.
#[actor(singleton, root, runtime::headless)]
pub struct HeadlessAudioCapability;
