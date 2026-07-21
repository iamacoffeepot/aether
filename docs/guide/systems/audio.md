# Audio

> **Decision status:** [ADR-0039](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0039-desktop-midi-synth-and-audio-sink.md)
> is Accepted. [ADR-0103](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0103-sampled-audio-track-playback-and-instrument-banks.md),
> [ADR-0104](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0104-scheduled-note-events.md),
> [ADR-0126](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0126-master-reverb-send.md), and
> [ADR-0127](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0127-per-note-pan-and-per-sender-gain.md) are Accepted
> and marked shipped. This page describes the current implementation, including
> places where it has moved past early ADR examples.

Audio is a native peripheral behind the singleton mailbox `aether.audio`.
Components send symbolic note events or file-backed playback requests; the
desktop substrate owns the output device, synth, mixer, and decoded PCM. The
mail handler never renders audio itself. It moves control events through a
bounded lock-free queue to the callback-owned mixer, while file reads, decode,
resampling, and SFZ assembly stay off the real-time path.

## Mental model

There are three lanes in one mixer:

- **Synth voices** come from immediate or scheduled note-on/off events. A voice
  uses one of the 11 compiled-in patches or a loaded sampled bank.
- **Tracks** are decoded WAV files. They live beside the voice pool, so note
  saturation cannot steal background music or ambience.
- **Master processing** adds a fixed-character reverb return, applies master
  gain, then soft-clips each output channel with `tanh`.

The sender in the mail envelope is part of audio state. Synth voices are keyed
by `(sender mailbox, instrument id, pitch)`, sender gain is keyed by sender
mailbox, and tracks are keyed by `(sender mailbox, lane, namespace, path)`.
Component senders therefore isolate one another without carrying a sender id in
the payload. Non-component sources collapse to mailbox id `0`; callers such as
MCP sessions should use distinct track `lane` strings when sharing a path.

`AudioCapability` is also an addressing marker available with the lightweight
`audio` feature. Native state, cpal, WAV decode, and the worker thread require
`audio-runtime`. A wasm guest can therefore name the capability and its kinds
without linking the native stack. See
[`audio/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/mod.rs) and the
feature declarations in
[`aether-audio/Cargo.toml`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/Cargo.toml).

## Public mail surface

Kind names are the wire contract; the Rust names in the second column are
payload types. Do not send a Rust type name as a kind name.

| Mail kind | Rust payload | Result and effect |
|---|---|---|
| `aether.audio.note_on` | `NoteOn` | fire-and-forget; allocates a voice |
| `aether.audio.note_off` | `NoteOff` | fire-and-forget; releases one matching voice |
| `aether.audio.set_master_gain` | `SetMasterGain` | `aether.audio.set_master_gain_result` / `SetMasterGainResult` |
| `aether.audio.set_reverb_send` | `SetReverbSend` | `aether.audio.set_reverb_send_result` / `SetReverbSendResult` |
| `aether.audio.set_sender_gain` | `SetSenderGain` | `aether.audio.set_sender_gain_result` / `SetSenderGainResult` |
| `aether.audio.schedule` | `Schedule` | `aether.audio.schedule_result` / `ScheduleResult` |
| `aether.audio.play_track` | `PlayTrack` | deferred `aether.audio.play_track_result` / `PlayTrackResult` |
| `aether.audio.stop_track` | `StopTrack` | fire-and-forget; fades a matching track out |
| `aether.audio.load_instrument` | `LoadInstrument` | deferred `aether.audio.load_instrument_result` / `LoadInstrumentResult` |

The exact fields and schemas live in
[`audio/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/kinds.rs).
The capability has 15 kinds total when the six reply kinds are counted.

### Notes and scheduling

`NoteOn` carries MIDI `pitch`, MIDI `velocity`, an `instrument_id`, and required
bipolar `pan`. Pan `0` is center, `-128` is hard left, and `127` is hard right;
the mixer uses a constant-power law. `NoteOff` carries only pitch and instrument
because pan is not voice identity.

Repeated note-ons with the same sender, instrument, and pitch **stack**. Each
creates an independent voice. Matching note-offs release the oldest unreleased
voice on that key, one at a time; an unmatched note-off is a silent no-op. The
runtime does not special-case velocity zero: it still allocates a
zero-amplitude voice, so send `NoteOff` when release is intended. The current
behavior is pinned in
[`runtime/synth.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/synth.rs),
even though some older comments describe retrigger or velocity-zero behavior
differently.

The active pool is capped at 128 voices. At capacity, a new note moves the
quietest current voice into a short forced-release list before entering the
pool. An unknown instrument id, or a sampled bank with no region covering the
requested pitch and velocity, warn-drops the note.

`Schedule` carries a vector of `ScheduledEvent`s. Offsets are
`at_millis` relative to the batch's receipt by the audio callback; the callback
converts them to its sample clock, preserving simultaneous events and firing
events within a render block at the due sample. Validation is atomic:

- the batch must be non-empty;
- it may contain at most 8,192 events;
- no event may be more than 600,000 milliseconds ahead; and
- the entire accepted batch occupies one event-queue slot.

Any validation failure rejects the whole batch. A full event queue also returns
`ScheduleResult::Err`; no prefix is admitted. See
[`runtime/schedule.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/schedule.rs)
and
[`runtime/handlers.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/handlers.rs).

### Instruments, tracks, and files

Built-in instrument ids are append-only and currently map as follows:

`0 sine_lead`, `1 square_bass`, `2 triangle`, `3 saw_lead`, `4 pluck`,
`5 piano`, `6 electric_piano`, `7 pad`, `8 kick`, `9 hat`, `10 snare`.

Reordering that table is a wire break. The source of truth is
[`runtime/instrument.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/instrument.rs).

`PlayTrack` and `LoadInstrument` address files as `namespace` plus `path` and
delegate reads to `aether.fs`; audio does not own a second namespace registry.
The v1 decoder accepts 16-bit integer and 32-bit float WAV PCM, averages
multichannel files to mono, and linearly resamples once to the device rate.
Decode happens on blocking-dispatch work, never in the callback. See
[`runtime/decode.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/decode.rs)
and [File I/O](file-io.md).

A track has a per-play linear gain and optional looping. Replaying the same
full key restarts it rather than stacking it; `StopTrack` with the same key
starts an approximately 5 millisecond fade. Tracks remain centered mono
sources and do not use the synth voice pool or per-sender synth gain.

`LoadInstrument` expects an SFZ file whose samples are WAV files in the same fs
namespace. The supported SFZ subset covers control/group/region inheritance,
key and velocity ranges, root pitch, sample paths, and loop points/mode;
unknown opcodes warn and are ignored. A successful load appends a bank after
the built-ins and returns its session-scoped `u8` id plus resident PCM bytes.
There is no unload or deduplication. Load order therefore matters, and ids do
not survive restart. Details are in
[`runtime/sfz.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/sfz.rs)
and
[`runtime/load.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/load.rs).

### Gain, pan, and reverb

Master gain clamps to `0.0..=1.0`. Reverb send also clamps to `0.0..=1.0` and
is dry by default; it feeds a fixed mono Freeverb-style network and adds the
wet return equally to left and right. Sender gain clamps to `0.0..=4.0` and
affects already-sounding synth voices on the next callback block. It does not
create named buses or affect track gain.

Voice output is split left/right by pan. A stereo device receives left on
channel 0 and right on channel 1; additional channels receive their average.
A mono device receives the pre-pan mono sum, so pan is intentionally inaudible.
The implementation is in
[`runtime/voice.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/voice.rs),
[`runtime/synth.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/synth.rs),
and
[`runtime/reverb.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/reverb.rs).

## Invariants and failure modes

- The handler-to-callback queue holds 1,024 `AudioEvent`s. Immediate notes,
  stops, and several callback registration/control events warn-drop on
  overflow. `Schedule` is the exception that reports queue overflow as `Err`.
- The current gain-control handlers ignore a failed queue push and still
  return the clamped `Ok` value. Track start and instrument registration can
  likewise log a full-queue drop after their deferred request later resolves
  `Ok`. Treat these replies as validation/load completion, not sample-level
  acknowledgement by the callback.
- File read, decode, parse, and sampled-bank assembly failures are returned in
  the corresponding result kind. `StopTrack`, `NoteOff`, and fire-and-forget
  note events have no error reply.
- Loaded PCM and sampled banks are resident for the session. Long tracks are
  decoded whole; there is no streaming decoder or bank eviction.
- The public float controls are not explicitly rejected for NaN by the
  handlers. Send finite gain and size values; the documented clamp ranges are
  meaningful only for finite input.
- The callback owns voice, schedule, gain, bank, track, and reverb state. Do
  not add blocking work, allocation-heavy transforms, or capability calls to
  its per-sample path.

## Chassis and boot behavior

The desktop chassis composes the full `AudioCapability`. It accepts
`AETHER_AUDIO_DISABLE=1` / `--audio-disable` and an optional requested sample
rate. Disabled audio, no output device, or an unsupported requested rate is
non-fatal to substrate boot: the capability stays registered in nop state.
Immediate notes then disappear, while reply-bearing handlers implemented by
that capability return an audio-not-initialised error.

The production headless chassis currently registers a small inline
`aether.audio` sink instead. It absorbs all audio mail and emits an error reply
only for `aether.audio.set_master_gain`; it does **not** synthesize the newer
reply kinds. Do not await schedule, track, instrument, reverb, or sender-gain
results there. The minimal hub and SubstrateHarness chassis do not compose the audio
capability. These current facts are visible in
[`headless/chassis.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis-headless/src/chassis.rs),
[`hub/chassis.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis-hub/src/chassis.rs),
and
[`substrate_harness/chassis.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-harness-substrate/src/chassis.rs).

## Where to change or extend it

- Change wire vocabulary in
  [`audio/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/kinds.rs),
  then update handlers, schemas, tests, and the governing ADR. Required fields
  such as `pan` are strict on structured input; omission is an encoding error.
- Add a built-in patch only by appending to `BUILTINS`; never reorder existing
  ids. Add sampled-bank behavior in `sample.rs`/`sfz.rs`, keeping decode off the
  callback.
- Change scheduling limits or admission in `runtime/schedule.rs` and
  `runtime/handlers.rs`; change sample-clock execution in `runtime/synth.rs`.
- Add callback DSP only with an explicit architecture decision. ADR-0126 is a
  narrow exception for one master effect, not an open effect graph.
- Keep desktop, nop-mode, and headless behavior aligned when adding a
  reply-bearing kind. A new desktop handler without a matching unsupported
  reply path otherwise becomes a settlement hang on non-audio chassis.
