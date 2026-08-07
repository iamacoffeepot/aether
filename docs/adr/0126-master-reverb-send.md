# ADR-0126: Master reverb send on the audio mixer

- **Status:** Accepted (shipped — master reverb send in `crates/aether-audio/src/runtime/reverb.rs`)
- **Date:** 2026-07-03

## Context

ADR-0039 set the audio substrate's governing principle: a dumb substrate that sums voices and applies master gain, with mixing, buses, and effects composed in user-space. It rejected substrate-owned effects (reverb, compressor, EQ) for v1 on the grounds that a user-space "effect component" — one that applies DSP to incoming notes before forwarding them — is cleaner, and committing to a specific effect set early locks the engine in. ADR-0103 kept that shape while adding the track lane and sampled-instrument banks, still summing everything flat into the cpal callback ahead of `master_gain` and the `tanh` soft clip.

That principle holds for most effects, but reverb is the case it does not cover. A user-space forwarding component can only shape the *events* going into the synth (velocity, timing, note choice) or the *dry samples* a source renders; it has no access to the summed post-mix signal on the audio callback, which is where a room lives. Every voice the synth renders is bone-dry — close-mic'd samples and oscillators summed flat — and the single biggest gap between an engine render and a professional-sounding track is that room. Live spine auditions against the sampled Salamander bank (ADR-0103) surfaced this repeatedly: the "smear" a listener asks of a misty/rainy piano texture is partly sustain pedal (reachable with deferred note-offs) and past that point is a room reverb, which no realization-side trick or event-level forwarding component can supply. The only place a master reverb can exist is where the samples are already summed — the cpal callback path in `crates/aether-capabilities/src/audio/runtime/`.

## Decision

Add a single fixed reverb on the master bus, tapped as a send, applied after the voice-plus-track mix and before the soft clip. This narrowly amends ADR-0039's "no substrate-side effects" stance for this one effect, and does not reopen the general bus/effects question:

- **One knob, one effect.** A master send level (`0.0..=1.0`) governs how much of the dry master feeds a fixed Freeverb/FDN-class reverb network whose room parameters (room size, damping) are compiled-in constants. There is no per-voice routing, no per-source send, no named buses, no runtime-tunable reverb parameters, and no second effect. The send level is the entire control surface.
- **Send, not insert.** The reverb returns a wet-only signal that sums back onto the dry master; at send `0.0` (the default) the output is bit-identical to today's dry mix, so the effect is inert until dialed. The substrate stays dumb everywhere the send is left at zero.
- **Lives on the callback.** The reverb runs in the synth's `fill` loop on the cpal callback thread. Its delay-line buffers are allocated once when the device rate is known (synth construction) and never on the hot path; per-sample work is a fixed constant (a fixed comb/allpass count), so cost is `O(frames)` per callback and buffer-size-independent, matching the voice mix's order.

The knob is a mail kind (`aether.audio.set_reverb_send`), mirroring `set_master_gain` end-to-end — kind, event-queue crossing, clamped-echo reply, and fail-loud `Err` on a chassis without audio — so the send is live-tweakable during an audition rather than a boot-time constant. This ADR governs the mechanism only; the reverb algorithm's constants and its mono-vs-stereo shape are implementation choices recorded in the closing issue's scope, not load-bearing wire commitments.

## Consequences

- The substrate now owns exactly one effect. This is a deliberate, bounded exception to ADR-0039 §2 and its "no substrate-side effects" rejection — narrowed to a fixed master send that cannot exist in user-space, not a reopening of the bus/mixer/effects topology those ADRs park. The "dumb substrate / composable user-space" principle still governs per-source dynamics, ducking, and cross-component routing.
- The audio wire vocabulary grows by one control kind and its reply (`aether.audio.set_reverb_send` / `_result`), following the `set_master_gain` shape. No existing kind changes.
- The mixer's signal chain gains a wet-return stage between the dry mix and the soft clip. At the default send of `0.0` the added stage is a no-op, so no existing render changes without an explicit `set_reverb_send`.
- The v1 reverb is mono-in / mono-out, consistent with the current mono mixer (ADR-0103 "Mono only"). If the mixer later goes stereo (e.g. per-note pan), the reverb wants a stereo version; that rides the stereo-mixer change rather than this one, and is not a precondition for the mono send shipping now.
- Fixed room constants mean the room is not tunable in v1. Runtime-tunable reverb parameters (decay, damping, pre-delay), multiple effects, and per-source sends remain parked as user-space / future substrate work, exactly as ADR-0039 left them.

## Alternatives considered

- **User-space effect component (ADR-0039's stated path).** Rejected for this effect: a forwarding component sees only the events or the pre-mix dry samples, never the summed master signal a room reverb must process. The one effect ADR-0039's escape hatch cannot supply is the one this ADR adds.
- **Config-only send (boot-time `AETHER_AUDIO_REVERB_SEND`).** Rejected as the sole surface: the motivating use is dialing the room live during an audition, which a boot constant cannot serve. A mail kind is live-tweakable; a config default could ride the derive-`Config` path later as a convenience without changing this decision.
- **Full bus/effects topology (named buses, per-source sends, an effect chain).** Rejected: exactly what ADR-0039 declined, and still the right call — it prescribes a mixer model the engine does not yet know it wants. This ADR stays a single fixed master send.
- **Insert (100% wet replace) instead of a send.** Rejected: a send keeps the dry signal intact and makes send `0.0` a true no-op, so the effect is inert by default and the substrate stays dumb until asked.
