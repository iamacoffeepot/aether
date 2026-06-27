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

// ADR-0121: the `aether.audio.*` mail vocabulary the cap owns, riding
// the always-on `audio` marker (not native-gated) so a wasm guest
// addressing the cap through the marker feature sees the types. The
// glob re-export surfaces every audio kind at `aether_capabilities::audio::*`
// for external callers.
pub mod kinds;
pub use kinds::*;

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity (always-on, outside the runtime gate). The audio kinds resolve
// through the `kinds` glob above; `ReadResult` is an `aether.fs` kind (a
// different cap) the audio cap receives as the track-load reply.
use crate::fs::ReadResult;

// `AudioConfig` rides through file root for chassis-bin consumers
// that build it from env (`from_env`) and pass it to
// `with_actor::<AudioCapability>(cfg)`. The config seam now lives under
// the `runtime` directory beside the rest of the runtime half, so the
// re-export sources through `runtime` and re-gates to `audio-native`
// (the `mod runtime;` gate) — wasm components opting into the marker-only
// `audio` feature don't need the config struct (sends are typed; config
// is the chassis's concern).
#[cfg(feature = "audio-native")]
pub use runtime::{AudioConfig, AudioConfigLayer, AudioOverlay};

/// `aether.audio` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`) plus
/// the per-handler `HandlesKind` markers, emitted always-on by `#[actor]` — so
/// a marker-only / wasm build names the cap without pulling cpal or the synth
/// pipeline. The state-bearing runtime (`runtime::AudioCapabilityState`, which
/// owns the cpal worker thread + the deferred-load bookkeeping) lives behind
/// the one `feature = "audio-native"` gate, so a marker-only build never names
/// `AudioCapabilityState` nor pulls the native audio stack through this cap.
#[actor(singleton)]
pub struct AudioCapability;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` or cpal/synth type — the
// handler/init ctx, the runtime state, the worker, the fan-out helpers, `Drop`,
// and the `#[runtime] impl` itself — lives in the `runtime` module below, gated
// once on `feature = "audio-native"`. The handler-signature kinds stay always-on
// at file root (the `kinds` glob + `crate::fs::ReadResult`) — the always-on
// `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The runtime half — the whole cpal/synth + `aether_substrate`-typed surface
// (imports, `AudioCapabilityState`, its helpers + `Drop`, `spawn_audio_worker`,
// `sender_mailbox_id`, and the `#[runtime] impl`) — lives in `runtime.rs`, gated
// once here on the media cap's own `audio-native` feature (it implies `native`,
// not the generic `runtime`).
#[cfg(feature = "audio-native")]
mod runtime;
