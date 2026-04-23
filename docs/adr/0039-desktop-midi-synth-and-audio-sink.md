# ADR-0039: Desktop MIDI synthesis and audio sink

- **Status:** Proposed
- **Date:** 2026-04-23

## Context

ADR-0002 named audio as one of the substrate's owned peripherals (alongside I/O and GPU), but nothing has shipped. The motivating forcing function is Claude-authored music: the harness is a natural fit for someone who can write symbolic music (note lists, chord progressions, rhythmic structure) but can't listen to audio. MIDI-level authoring keeps Claude in the loop for composition, phrasing, and arrangement while delegating synthesis to the native layer — the same split the substrate already uses for GPU rendering (components emit `DrawTriangle`, substrate runs wgpu).

A one-shot authoring experiment on 2026-04-23 closed the loop well enough to commit to MIDI as the authoring format:

- Claude wrote a 12-bar ABC-notation waltz in D minor.
- `abc2midi` compiled it to a standards-compliant `.mid` file.
- A small pure-Python sine synth (`/tmp/render_sine.py`) rendered it to 16-bit PCM with a hand-rolled ADSR envelope — no SoundFont involved.
- Gemini 2.5 (via Google's Generative Language API) returned a useful critique of the PCM: correctly identified the key, the synth's timbre, and the metronomic rhythm, though it misidentified the meter as 4/4 (the 3/4 waltz feel wasn't audible without chord accompaniment).

The experiment also settled a scope question: the audio-critique loop is **not** part of this ADR. The user will act as the judge initially ("I'll just say human things"), which defers every design question that a `capture_audio` MCP tool would introduce (API-key cost surface, schema for structured critique, rendering-offline vs. real-time capture).

Three forces shape what this ADR *does* commit to:

1. **Claude-authorable end-to-end.** The authoring format must be something Claude can produce directly (ABC → MIDI events, or event lists written as Rust literals), not a format that requires a DAW.
2. **Dumb substrate / composable user-space.** The render-sink / camera-sink pattern has paid off: a flat output surface with composition logic living in components has let us add topdown and orbit cameras, a player, a sokoban level, and a tic-tac-toe board without substrate churn. Audio should inherit this shape — no substrate-side bus model, no substrate-side effects, no substrate-side mixer.
3. **Desktop-only, like depth testing.** Headless and hub chassis have no audio device; they should reject audio-control mail loudly, matching how they reject `capture_frame`, `set_window_mode`, and friends (ADR-0035).

## Decision

MIDI event semantics are the wire format. The desktop chassis owns a `cpal` output stream fed by a substrate-resident synth. Components emit to an `aether.audio.*` sink; substrate sums voices, applies master gain, and pushes samples to the device.

### 1. Wire shape

Two hot-path kinds plus one control kind for v1:

- `aether.audio.note_on { pitch: u8, velocity: u8, instrument_id: u8 }` — cast-shaped (`#[repr(C)]`); fire-and-forget.
- `aether.audio.note_off { pitch: u8, instrument_id: u8 }` — same.
- `aether.audio.set_master_gain { gain: f32 }` — control-plane; normalised 0.0–1.0 (values above 1.0 clamped).

The substrate keeps a voice table keyed by `(sender_mailbox_id, instrument_id, pitch)`. `note_on` allocates a voice; `note_off` matches on all three and triggers the voice's release phase. Keying by sender means a component can't accidentally kill another component's note with the same pitch, and a future "stop all notes from X" operation falls out for free.

Sample-exact scheduling (a `delay_samples: u32` field) is deliberately omitted. Events fire at mail-arrival time, accepting ~16ms tick-rate jitter. Sufficient for melodic content; tight percussion is future work.

MIDI CCs (sustain pedal, pitch bend, modulation wheel, channel volume) are not included in v1. Expression comes from velocity alone.

### 2. Instrument registry

Substrate ships a small fixed set of named instruments (`"sine_lead"`, `"square_bass"`, `"triangle"`, `"pluck"`, `"noise_perc"` — exact set TBD at PR time). Each is a code-level `Instrument` implementation; `instrument_id: u8` indexes the registry at boot. Components learn the id of an instrument by name through a lookup mail (`aether.audio.resolve_instrument { name: String }` → `InstrumentId { id: u8 }`), analogous to how mailbox names become ids today.

Runtime-defined patches (mailed instrument definitions) are explicitly deferred. When we need one, the shape will be a `Patch` kind carrying oscillator + envelope + filter config, interpreted by a generic voice; no substrate churn — just one new kind and one new dispatch branch in the synth.

### 3. Mixing topology

The substrate synth sums every allocated voice flatly into the cpal stream. There is no bus model, no named channels beyond the master, no per-source gain at the substrate level. Per-source dynamics are expressed through MIDI velocity; cross-component routing (a music component that ducks when sfx fire, a mixer that routes bass to a compressor bus) is user-space work — a Claude-authored component that other components mail into, which forwards `aether.audio.note_on` downstream after applying its own policy.

This mirrors the render pipeline: scene composition is component-authored; the substrate sees only `DrawTriangle`. Audio composition is component-authored; the substrate sees only `NoteOn`/`NoteOff`.

### 4. Chassis support

Desktop chassis only. Headless and hub chassis register the `aether.audio.*` sink as a nop (matching how `render` and `camera` are nop'd on headless per ADR-0035) for the hot-path kinds so components don't warn-storm `engine_logs`; `set_master_gain` replies with `Err { error: "unsupported on <chassis>" }` to fail loudly when an agent tries to control audio on a chassis that can't produce it.

### 5. Boot-time overrides

- `AETHER_AUDIO_DISABLE=1` — desktop chassis skips cpal stream construction (useful for CI / headless-like desktop runs). All audio sinks become nops.
- `AETHER_AUDIO_SAMPLE_RATE=<hz>` — request a specific sample rate from cpal (default: device preference, typically 44100 or 48000).

## Consequences

### Positive

- **Claude can author music end-to-end.** Compose → ABC or JSON event list → component emits MIDI → substrate synthesises → user hears. No external DAW, no soundfont wrangling, no human-in-the-loop beyond "is this good?"
- **Substrate stays dumb.** No bus logic, no effects, no mixer — all composable in user-space. A music component can grow from "one melody" to "multi-track with ducking" without touching native code.
- **Deferred additions are backwards-compatible.** Adding `delay_samples` is a field widen on a cast kind (not breaking). Adding patch-based instruments is a new kind, not a wire change. Adding CCs is a new kind. Nothing in v1 forecloses a bigger synthesis surface later.
- **User-space velocity covers 80% of dynamics work.** The "should I add bus gain?" question mostly evaporates when components can emit at different velocities.

### Negative

- **v1 timbre is toy.** Hand-rolled sine / square / triangle oscillators with ADSR sound like chiptune. Good enough for testing the loop; probably grating for anything musical we'd want to keep. The patch-based synthesis follow-up lifts the ceiling.
- **No sample-exact scheduling.** Tick-rate jitter is audible for percussion with ≤30ms spacing. V1 limitation; the field widen when we need it is cheap.
- **Timing + concurrency of the audio callback is a new surface.** cpal runs the audio callback on a realtime-priority thread that can't block on mail-queue locks. The synth's voice table needs a lock-free or mailbox-queued approach; getting that wrong produces audio glitches that `engine_logs` can't show. Well-trodden territory (most Rust audio libraries solve it), but new to aether's substrate.
- **Mixer-as-component has a learning curve.** The first Claude-authored game that wants "mute music, keep sfx" has to write (or reuse) a mixer component. A substrate-side bus would have been simpler for that specific case.

### Neutral

- **Guest SDK unchanged.** `Sink<K>` + `ctx.send(&sink, &NoteOn { .. })` works identically to render mail. No new host-fn.
- **Kind manifest unchanged in shape.** Three new kinds added to `aether-kinds`; that's a data change, not a structural one.
- **Scheduling unchanged.** Audio mail flows through the same ADR-0038 actor-per-component path as everything else. The synth is a chassis peripheral — not a component — so it reads from a sink queue the chassis installs, not a per-component mailbox.

## Alternatives considered

- **Raw PCM samples on the wire.** Components emit f32 audio buffers to an audio sink; substrate mixes and plays. Rejected: makes Claude author at the *sample* level, which is absurd; bandwidth is orders of magnitude higher than MIDI events; loses all symbolic structure that makes composition tractable.
- **SoundFont-based synth (`rustysynth`).** Sounds like real instruments immediately. Rejected for v1: user pushed back on SF2 as a dependency shape; adds a ~6MB+ asset to ship; licensing on free soundfonts ranges from clean to questionable. Revisit if hand-rolled instruments fail the "does this sound tolerable?" bar.
- **`midir` to OS MIDI bus (fluidsynth / GarageBand).** Substrate emits MIDI bytes; OS handles synthesis. Rejected: punts the aether synth box entirely; no portable path for "press a button and hear aether make a sound"; cross-platform OS MIDI is itself a can of worms.
- **Substrate-owned named buses (`"music"`, `"sfx"`, `"ambient"`).** Classic game-audio shape. Rejected: prescribes a mixer topology when we don't know what we want; can be built in user-space with zero substrate change; introduces a channel-name vocabulary that games would inherit whether they want it or not.
- **Substrate-owned effects (reverb, compressor, eq).** Rejected for v1: same reason as buses — user-space can compose them, and committing to a specific effect set early locks us in. A user-space "effect component" that applies DSP to incoming notes before forwarding is cleaner.
- **Audio-critique MCP tool (`capture_audio` + Gemini integration) as part of this ADR.** Rejected: adds API-key management, rendering-offline concerns, structured-output schema, and cost-per-call surface — orthogonal to whether audio works. User acting as judge removes the pressure.
- **MIDI channel vocabulary (0–15) with program-change-per-channel.** Standard MIDI shape. Rejected: adds a stateful channel→instrument mapping the substrate has to track, for no gain over "`NoteOn` names its instrument directly." Simpler cast shape wins when the MIDI-standard baggage isn't buying us compatibility.

## Follow-up work

- **v1 PR**: cpal stream, `Instrument` trait + 4–5 built-ins, voice table, `note_on` / `note_off` / `set_master_gain` kinds, `resolve_instrument` lookup, desktop/headless/hub chassis wiring.
- **Parked, not committed**: `delay_samples` on `NoteOn` for sample-exact scheduling.
- **Parked, not committed**: runtime-defined patches (mail a `Patch` kind, synth grows a generic voice).
- **Parked, not committed**: `capture_audio` MCP tool + automated critique pipeline (Gemini or local captioner). Revisit when manual listening becomes a bottleneck.
- **Parked, not committed**: CC support (sustain pedal, pitch bend, modulation) for expressive playback.
- **Parked, not committed**: stereo panning / spatial audio; effects (reverb, compressor, EQ); aftertouch.
- **Authoring-side, not substrate**: ABC → MIDI-event utility, so Claude can author in ABC and the resulting kind-literal events ride through the sink directly.
