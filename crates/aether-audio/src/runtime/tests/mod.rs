// `sender.push(...).unwrap()` reads as test setup — the channel
// is local and never full / closed during the test. `.expect`
// per call would be pure noise.
#![allow(clippy::unwrap_used)]

use super::super::*;
use super::decode;
use super::event::new_event_channel;
use super::instrument::{
    Adsr, BUILTINS, PARTIAL_COUNT, PartialBankDef, PitchSweep, VoiceDef, Wave, builtin_count, builtin_names,
};
use super::sample::{SampleBank, SampleLoop, SampleRegion, SampleVoice, assemble_bank};
use super::sfz::{SfzLoop, SfzRegion};
use super::synth::Synth;
use super::voice::{
    MAX_VOICES, OscVoice, PartialBankVoice, STEAL_RELEASE_SECS, VoiceKernel, build_builtin_kernel, voice_seed,
};
use super::*;
use aether_substrate::Registry;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::registered_ref;
use std::sync::Arc;

const TEST_RATE: f32 = 48_000.0;

/// Mono ramp samples for an in-memory WAV fixture.
fn ramp(len: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    (0..len).map(|i| (i as f32 / len as f32) - 0.5).collect()
}

mod cap_close;
mod departure;
mod instrument;
mod synth_voice;
mod track;
