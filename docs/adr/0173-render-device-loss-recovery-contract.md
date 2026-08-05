# ADR-0173: Render device-loss recovery contract

- **Status:** Proposed
- **Date:** 2026-08-05

## Context

The pumped render actor owns one wgpu device and every object created from it. A device loss invalidates the shared `RenderGpu`, built-in pipelines and targets, realized texture and geometry resources, authored-program pipelines and dispatch caches, timing instrumentation, desktop surfaces, and capture state. The current runtime boots `gpu: Option<RenderGpu>` once and treats a failed poll as a warning followed by continued use; it has no loss generation or replacement transaction.

The session-scoped registries already retain enough CPU state to reconstruct sampled textures and geometry without changing their public identifiers. Authored programs retain their validated plan but not their WGSL, so recovery requires retaining that source as well. Writable program-output textures are the exception: ADR-0170 deliberately creates them without staged pixels and excludes readback from the ordinary render loop, so their last GPU contents cannot be reconstructed.

Recovery must also preserve Aether's mail semantics. A frame or capture whose submission outcome is unknown must not be replayed, because duplicate GPU work and duplicate after-mail release are observably wrong. Desktop recovery has the additional constraint that every retained window surface must be compatible with one replacement adapter and format before any new device state becomes visible. The policy must terminate after failure rather than spin inside the pumped actor.

## Decision

Device-loss recovery is internal to `aether.render`. It does not add a render-generation kind, wire signal, or actor callback. The render runtime records device generations and moves explicitly between usable, recovery-pending, and unusable states. Loss callbacks carry their generation; callbacks from an older generation are ignored.

The first frame after a known loss performs one replacement transaction for that lost generation. Offscreen recovery acquires a replacement device, constructs a fresh `RenderGpu`, invalidates every device-bound registry realization, rebuilds recoverable resources, and commits the new generation only when the transaction is complete. Desktop recovery additionally chooses an adapter compatible with the first canonical retained window, validates the shared format and required usage across every retained window, and recreates all surfaces before atomically installing the replacement device, target map, and overlay state. A partially rebuilt generation is never published.

If that one replacement transaction fails, the render capability becomes unusable for the remainder of the session. Request/reply GPU operations and captures return `Err`; fire-and-forget draw, dispatch, update, and destroy work is warning-dropped. The transition emits one structured error and does not retry or spin. A successfully installed replacement generation receives the same one-attempt policy if it is later lost.

Recovery preserves all public session identifiers. Sampled textures and geometry are rebuilt from their retained CPU bytes. Authored programs retain their WGSL alongside the validated plan and rebuild their pipelines and dispatch caches. A program that fails to compile on the replacement device is quarantined under its existing identifier; later dispatches to it warning-drop without preventing unrelated programs or frames from recovering. Writable textures preserve their identifiers but restart with cleared contents. This is an explicit device-loss exception to ADR-0170's ordinary writable-texture pixel persistence. Actors redispatch their immediate-mode authored programs on the next ordinary repaint.

Transient views, the prior submission handle, device-bound timing queries, and per-program dispatch caches are discarded. Already folded timing samples remain available. Dispatches known not to have recorded may survive the transaction; work whose submission outcome is unknown is not replayed.

A pending capture that is ready but has not begun recording may survive one successful recovery and record once. Loss during submission, polling, mapping, or readback completes that capture with `Err`; it does not replay the frame or release after-mails twice. Stale completion callbacks from an older generation have no effect.

## Consequences

- Device loss becomes a bounded state transition instead of an indefinitely damaged render loop. Failed recovery is visible and terminal for that session.
- Existing actor mail and identifier contracts remain stable. No capability kind, FFI, or wire-format growth is required.
- The registries retain additional recovery data, principally authored WGSL and staged resource bytes. Writable textures intentionally trade content survival for avoiding routine GPU readback and a new actor-visible protocol.
- Recovery work lands serially: first make registry state rebuildable without visual change, then recover the offscreen runtime, then recover retained desktop surfaces.
- Offscreen recovery must prove the ordinary puppet image is unchanged before loss and restored after loss. Desktop recovery must separately prove all retained windows resume. Both visual slices require owner inspection before merge.
- Program rebuild failures are isolated to those programs. Device or surface replacement failure is not isolated: the shared render capability becomes unusable because publishing a partial generation would violate cross-resource identity and multi-window consistency.

## Alternatives considered

- **Publish a render-generation or recovery signal to actors.** Rejected: immediate-mode actors already redispatch on repaint, and a new public protocol would make an internal substrate failure part of every renderer consumer's state machine.
- **Read back and replay writable texture contents.** Rejected: it adds routine GPU synchronization and storage to preserve content that ADR-0170 intentionally keeps GPU-only.
- **Retry replacement indefinitely or with backoff.** Rejected: a pumped actor must not spin or accumulate work behind a device that cannot be restored.
- **Abort the substrate when replacement fails.** Rejected: other capabilities can remain useful, and request/reply render operations can report the terminal failure directly.
- **Delete failed programs or allocate new resource identifiers.** Rejected: it breaks session identity and turns a recoverable device event into actor-visible registry churn.
- **Replay frames or captures with unknown submission status.** Rejected: duplicate GPU effects and duplicate after-mail release are worse than an explicit `Err`.
