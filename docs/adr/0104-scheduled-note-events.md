# ADR-0104: Scheduled note events for the audio capability

- **Status:** Proposed
- **Date:** 2026-06-12

## Context

The audio capability plays notes on receipt: `aether.audio.note_on` / `note_off` (ADR-0039) admit a voice the moment the cpal callback drains the event queue. Mail delivery carries no timing, so a batch of note mail lands as one cluster chord — the relative timing that makes a melody a melody has nowhere to ride. The only single-call route to sequenced audio is rendering a tune to a WAV outside the engine and playing it through the track lane (ADR-0103 §3), which gives up the synth's instruments, per-note dynamics, and any ability to vary the music at runtime.

Senders that want live sequencing today must hand-pace one mail per beat. That timing is hostage to the sender's scheduling (an MCP round-trip per note, or a component handler woken at tick granularity), and 60 Hz ticks quantize to ~16 ms — audibly sloppy for music. Meanwhile the audio callback already owns the one clock that matters: it renders a known number of frames at a known sample rate, so "play this 3 seconds from now" is exactly representable as a frame count.

Two constraints shape the answer. The scheduler must live where the samples are rendered — any timing decided outside the callback re-quantizes to the block or tick that delivered it. And the event channel between the cap and the callback is a bounded lock-free queue (ADR-0039): a tune must not consume one queue slot per note.

## Decision

Schedule inside the synth, against the audio frame clock; carry a whole tune as one mail.

A new kind, `aether.audio.schedule`, carries a batch of timed events. Each event is an `at_millis` offset plus a note payload (the fields of `note_on` or `note_off`). The handler validates the batch synchronously — an events-per-batch cap and a horizon cap, rejecting the whole batch atomically on any invalid entry — and replies `aether.audio.schedule_result` in-handler (`Ok { accepted }` / `Err { error }`), the cheap-verdict-then-async-execution split. The accepted batch crosses the event queue as a single `Schedule` event.

The synth keeps a running frame counter and a min-heap of pending events ordered by due frame (ties broken by batch order). When the callback drains a `Schedule` event it converts each `at_millis` to an absolute due frame — offsets are relative to the batch's arrival at the callback, so every event in a batch shares one timebase and chords stay chords. The render loop pops due events at the frame they fall on and routes them through the existing note-on / note-off paths: scheduled notes allocate from the same voice pool, obey the same steal policy, and key note-off matching by the scheduling sender, exactly as if the mail had arrived at that instant.

Scheduling is sample-accurate by construction; nothing is promised about cross-batch alignment beyond the shared receipt timebase within a batch. There is no cancellation in v1 — the horizon cap bounds how much future a sender can park, and the pending heap self-bounds by draining as it plays.

## Consequences

- A multi-second tune is one `send_mail` item: an agent over MCP, or a score-player component, dispatches a batch and the engine renders it with sample-accurate relative timing.
- The synth gains a frame clock and a pending heap — both callback-owned, no new locking; one queue slot per batch keeps the bounded event queue safe from tune-sized fan-out.
- Batch validation is loud and atomic: a malformed event rejects the whole batch in the synchronous reply, so a score player can trust that an `Ok { accepted }` batch plays in full.
- The within-batch timebase (receipt at the callback) means two batches sent back-to-back are *not* phase-aligned to each other; long pieces that need seamless continuation must ride one batch or accept the seam. Cross-batch alignment (a shared transport clock, tempo maps) is future work this ADR neither builds nor forecloses.
- No cancellation in v1: a dispatched batch plays out. A `schedule_cancel` is additive later if a need appears.
- The kinds ride the substrate vocabulary (`aether-kinds`), so `describe_kinds` documents them to MCP callers automatically.

## Alternatives considered

- **General delayed mail at the mail layer** — deliver any kind at a future time. Tick-granular (~16 ms jitter), an engine-wide semantics change for a need only audio has demonstrated; if a non-audio need appears it is its own ADR.
- **A `play_at` field on `note_on` itself** — no batch atomicity, one queue slot per note against a bounded queue, and each note re-anchors its own receipt time so a chord's notes can skew across callback blocks.
- **Sequencing in a wasm component (tick-driven score player)** — works today with no engine change, but quantizes every note to the tick and pays a mail dispatch per note; the component remains the right home for *what* to play (reading scores, looping, reacting), with this kind as its timing-accurate output.
