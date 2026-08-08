# ADR-0127: Per-note pan and per-sender gain for the synth mixer

- **Status:** Accepted (shipped — per-note pan and per-sender gain in `crates/aether-audio/src/runtime/voice.rs` and `crates/aether-audio/src/runtime/synth.rs`)
- **Date:** 2026-07-03

## Context

ADR-0039 §3 (Mixing topology) set the substrate synth to sum every allocated voice flatly into one point: no bus model, no per-source gain at the substrate level, and dynamics carried by MIDI velocity alone. Stereo panning was parked in the same ADR's follow-up list. The cpal output path is already multi-channel — the render loop computes one mono `sample` per frame and writes that identical value to every channel of the device buffer — so the whole mix lands on the center point regardless of how many voices sound at once.

Live listening on multi-voice piano pieces has hit the limit that deferral anticipated. Concurrent voices with no spatial or level separation read as blended and hard to pick apart, and the only balance knob a score has — note velocity — conflates loudness with timbre: in the partial-bank voices velocity drives a brightness tilt, and in sampled banks it selects the region layer, so turning a voice down with velocity also changes how it sounds. A score needs to place the melody above the accompaniment and spread voices across the stereo image without touching their timbre.

Two balance controls are missing, at two different scopes. Placement is inherently per-note: each note lands somewhere in the image. Level trim is inherently per-sender: a voice line (one score-player component, one instrument) wants to sit at a fixed relative level across all its notes, set once. ADR-0104's scheduled events mirror the `note_on` field set exactly, so a per-note field has to ride the scheduled shape too or scheduled melodies keep only the center point.

## Decision

Add per-note pan and per-sender gain to the substrate synth, and make the mixer place voices in stereo instead of collapsing to mono.

### Per-note pan

`aether.audio.note_on` and `aether.audio.schedule`'s `ScheduledNote::On` gain a `pan: i8` field. The value is bipolar: `0` is center, `-128` hard left, `127` hard right, mapped to `[-1.0, 1.0]` at the synth. `i8` keeps `NoteOn` cast-shaped (a fourth one-byte field beside `pitch` / `velocity` / `instrument_id`) and preserves every derive the kind carries today (`Pod`, `Zeroable`, `Eq`, `Default`) — and `Zeroable`/`Default` land on `0` = center, so the zero value is the current behavior.

Pan is a render property of the voice, not part of its identity: the voice key stays `(sender_mailbox, instrument_id, pitch)`, so `note_off` (which matches on that key) is unchanged and carries no pan. A voice captures its pan at `note_on`; the synth precomputes the per-voice left/right gains once and the per-sample path stays multiply-only.

The field is required on the wire. The schema codec is strict — a JSON `note_on` that omits `pan` fails to encode with a loud `MissingField`, not a silent default. That is the right failure mode here: there are no persisted scores or stored `note_on` bytes anywhere (every sender is live and every in-tree consumer recompiles against the kind crate), so the only thing a required field breaks is a hand-written call that forgets it, which should be told rather than silently centered.

### Per-sender gain

A new control kind `aether.audio.set_sender_gain { gain: f32 }`, replying `aether.audio.set_sender_gain_result` (`Ok { applied_gain }` / `Err { error }`), sets a linear level trim keyed by the envelope sender — the same `sender_mailbox` component that keys voices. The synth holds a `HashMap<MailboxId, f32>` (absent = unity `1.0`) and multiplies each voice's contribution by its sender's gain at mix time. The lookup is resolved once per `fill` block per voice, not per sample, so the gain is live — a `set_sender_gain` ducks or lifts a sender's already-sounding voices, matching `set_master_gain`'s live semantics — without a hash probe in the hot loop.

`gain` clamps to `0.0..=4.0`. Values above unity are allowed on purpose: lifting the melody above the accompaniment is the motivating use, and the mixer's existing `tanh` soft clip catches any resulting overshoot. This is a wider range than `set_master_gain`'s `0.0..=1.0` clamp by design — master gain is the final limiter into the device, per-sender gain is a relative balance trim behind it.

Scheduled notes inherit their scheduling sender's gain for free: they allocate through the same `trigger_note_on` path with the scheduling `sender_mailbox`, so the mix-time lookup finds the same table entry. Like `set_master_gain`, both are desktop-only — a nop chassis (headless / hub / disabled / no device) replies `Err`.

### Stereo mixer

The render loop accumulates a left and a right accumulator per frame instead of one mono `sample`. Each voice's mono output is scaled by its sender gain, then split by a constant-power pan law (`gain_l = cos(θ)`, `gain_r = sin(θ)`, `θ = (pan_norm + 1) · π/4`), and summed into the two accumulators. Master gain and the `tanh` soft clip apply per channel. Channel handling: a ≥2-channel device takes left on channel 0, right on channel 1, and the mono average on any further channels; a 1-channel device sums left+right back to mono (pan is then inaudible, which is correct for a mono sink).

Track lanes (ADR-0103) keep their own per-play `gain` and are out of scope for pan in this ADR — a track and a sampled instrument remain mono point-sources because ADR-0103's decode still downmixes to mono (stereo persistence stays parked there). Pan can position a mono voice in the image; restoring true stereo width for tracks and sample banks is the separate un-parking of ADR-0103's stereo lane.

This amends ADR-0039 §3: the substrate now applies a per-sender level trim and per-note stereo placement, where §3 committed to a flat mono sum with velocity-only dynamics. ADR-0039's broader stance — no substrate bus model, no substrate effects, cross-component routing in user-space — still holds; this is per-source gain and placement, not a bus graph.

## Consequences

- A score gains real balance and width: the melody sits above the accompaniment via `set_sender_gain`, and voices spread across the image via per-note `pan`, both without perturbing timbre the way velocity does.
- The wire change is a required-field add on a cast kind plus one new control kind. A `note_on` / scheduled `On` that omits `pan` fails loudly at encode; every in-tree consumer recompiles against the kind crate, and `describe_kinds` surfaces the new field and kind to MCP callers automatically.
- The synth grows a per-sender gain table bounded by the number of live senders, and the render loop grows a per-channel split (two accumulators, a per-voice pan-gain pair, a per-block gain resolve). No new locking — the table and the pan state are callback-owned like the voice pool.
- Mono sinks are unaffected (pan collapses back to mono). Per-sender gain sent from an MCP session collapses to `MailboxId(0)` — all MCP-originated notes share one sender entry, so from MCP `set_sender_gain` is effectively a single extra global knob; real per-sender balance needs distinct component senders (a score-player per line), which is the intended shape.
- Tracks and sampled instruments stay mono point-sources; pan places them but cannot restore stereo width until ADR-0103's stereo lane is un-parked. The `tanh` soft clip now runs per channel rather than once per frame.

## Alternatives considered

- **`pan: f32` in `[-1.0, 1.0]`** — the natural synth representation, but `f32` is not `Eq`, so it would strip `Eq` from `NoteOn`, `ScheduledNote`, and `ScheduledEvent` and force `f32`-padding into the cast layout. `i8` gives 255 placement steps (inaudibly fine) and preserves every derive; the pan law reads the normalized float internally.
- **`pan: u8` on the MIDI CC10 convention (64 = center)** — keeps an unsigned byte, but `Zeroable`/`Default` = `0` would then mean hard-left, a surprising zero value. Bipolar `i8` makes the zero value center, which is the current behavior.
- **A per-note gain field instead of per-sender** — re-conflates loudness with the individual note and gives no set-once balance; the felt need is a fixed relative level for a whole voice line, which is sender-scoped.
- **Per-sender gain as a boot config knob** — senders are runtime `MailboxId`s assigned as components load; they cannot be named at boot, so a static config cannot address them.
- **Capture sender gain at `note_on` rather than resolving live** — simpler (no per-block resolve), but a `set_sender_gain` would not duck currently-ringing voices, diverging from `set_master_gain`'s live behavior for no real gain.
- **Keep pan and gain in user-space (a mixer component that applies them and forwards `note_on`)** — ADR-0039's original position. The substrate already sums into a stereo device, so a pan law and a gain multiply there are a few lines on the hot path; requiring every score author to wrap a mixer component just to place or trim a voice is disproportionate to that cost.
