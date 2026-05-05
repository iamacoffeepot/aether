//! Shared frame-loop policy helpers (issue 427).
//!
//! Three chassis binaries — desktop, test-bench, headless — drive a
//! `Mailer::drain_all_with_budget` per frame and, every 120 frames,
//! push a `FrameStats` observation to the broadcast mailbox. Pre-issue
//! 427 each chassis open-coded the budget constant, the wedge / death
//! handling, and the frame-stats emission. Any change to the wedge
//! message, abort policy (ADR-0063), or stats cadence had to be made
//! in three places.
//!
//! This module owns the policy. The helpers take only the data they
//! touch (`&Mailer`, `&HubOutbound`, mailbox / kind ids) so they're
//! callable from any chassis without threading a chassis handle
//! through. Behaviour matches the pre-refactor binaries exactly:
//!
//! - `drain_or_abort` runs the same `drain_all_with_budget` call,
//!   logs structured deaths, and routes wedges / deaths through
//!   `lifecycle::fatal_abort` with the same reason format strings.
//! - `emit_frame_stats` does the 120-frame gate inside the helper —
//!   chassis call sites become unconditional.
//!
//! `WORKERS` deliberately stays chassis-side. Post-ADR-0038 it's
//! declarative (the wire-stable `EngineInfo.workers` field, retained
//! for compatibility — the scheduler doesn't read it). It's not
//! actual loop policy and shouldn't be promoted into a shared
//! module just because every chassis happens to set the same value.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_data::encode;
use aether_kinds::FrameStats;

use crate::lifecycle;
use crate::mail::{Mail, MailboxId};
use crate::mailer::Mailer;
use crate::outbound::HubOutbound;
use crate::scheduler::DrainSummary;

/// Frame-stats emission cadence. Hardcoded for v1; an env knob is
/// deferred until a forcing function arrives. 120 frames at 60 Hz is
/// 2 s — frequent enough for a Claude session to see liveness via
/// `receive_mail`, sparse enough to stay out of the engine_logs
/// signal-to-noise budget.
pub const LOG_EVERY_FRAMES: u64 = 120;

/// ADR-0063 fail-fast budget for the per-frame drain barrier. A
/// dispatcher that doesn't quiesce within this window is treated as
/// wedged: the substrate logs, broadcasts `SubstrateDying`, and
/// exits via `lifecycle::fatal_abort`. 5 s is patient enough that
/// ordinary frames don't trip it even on slow first-load compiles,
/// short enough that an operator staring at a frozen window gets a
/// clean exit instead of a multi-minute wait.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Drain every live component's inbox under `DRAIN_BUDGET`. On
/// wedge or any death, log the diagnostic and route through
/// `lifecycle::fatal_abort` with the substrate's standard reason
/// format — pre-issue-427 each chassis duplicated this block; the
/// helper is the single owner.
///
/// Wedge wins over deaths: if both are present in the same drain,
/// the wedge aborts first because the wedged dispatcher is the
/// active hazard (it's still running and may be holding state we
/// care about). Deaths are logged structurally before the abort so
/// `engine_logs` carries the per-mailbox detail even when the
/// reason string only quotes the first one.
pub fn drain_or_abort(queue: &Mailer, outbound: &HubOutbound) {
    let summary = queue.drain_all_with_budget(DRAIN_BUDGET);
    if let Some(reason) = abort_reason(&summary) {
        lifecycle::fatal_abort(outbound, reason);
    }
}

/// Wait for every frame-bound capability's inbox to drain under
/// `DRAIN_BUDGET` (ADR-0074 §Decision 5). Mirrors [`drain_or_abort`]
/// but works on the per-mailbox pending counters
/// [`crate::ChassisCtx::claim_frame_bound_mailbox`] collected for the
/// chassis (snapshotted by drivers via
/// [`crate::chassis_builder::DriverCtx::frame_bound_pending`]).
///
/// Component drain runs first because component dispatchers are the
/// upstream of capability mail; running it second would let
/// component-emitted mail land in capability inboxes after we
/// already cleared them. Order: component drain → frame-bound drain
/// → render submit. Each chassis main calls these in that order.
///
/// Empty `pending` is a fast no-op — chassis without frame-bound
/// capabilities (today: headless, hub) call this every frame at
/// zero cost.
pub fn drain_frame_bound_or_abort(pending: &[(MailboxId, Arc<AtomicU64>)], outbound: &HubOutbound) {
    if pending.is_empty() {
        return;
    }
    let deadline = Instant::now() + DRAIN_BUDGET;
    loop {
        let mut still_pending: Option<(MailboxId, u64)> = None;
        for (mbox, counter) in pending {
            let v = counter.load(Ordering::Acquire);
            if v > 0 {
                still_pending = Some((*mbox, v));
                break;
            }
        }
        match still_pending {
            None => return,
            Some((mbox, count)) => {
                if Instant::now() >= deadline {
                    let reason = format!(
                        "frame-bound dispatcher wedged: mailbox={mbox} pending={count} waited={DRAIN_BUDGET:?}"
                    );
                    lifecycle::fatal_abort(outbound, reason);
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        }
    }
}

/// Compute the `fatal_abort` reason string for a drain summary, or
/// `None` if the drain quiesced cleanly. Factored out of
/// `drain_or_abort` so unit tests can pin the wedge / death message
/// format without going through `lifecycle::fatal_abort` (which
/// calls `std::process::exit`).
///
/// Wedge takes precedence over deaths when both are present —
/// matches `drain_or_abort`'s ordering.
fn abort_reason(summary: &DrainSummary) -> Option<String> {
    if let Some((mailbox, waited)) = summary.wedged {
        return Some(format!(
            "dispatcher wedged: mailbox={mailbox} waited={waited:?}"
        ));
    }
    if let Some(first) = summary.deaths.first() {
        for d in &summary.deaths {
            tracing::error!(
                target: "aether_substrate::lifecycle",
                mailbox = %d.mailbox,
                mailbox_name = %d.mailbox_name,
                last_kind = %d.last_kind,
                reason = %d.reason,
                "component died; substrate aborting (ADR-0063)",
            );
        }
        return Some(format!(
            "component died: {} (kind {}) — {}",
            first.mailbox_name, first.last_kind, first.reason,
        ));
    }
    None
}

/// Emit a `FrameStats` broadcast every `LOG_EVERY_FRAMES` frames.
/// The cadence gate lives inside the helper so chassis call sites
/// are unconditional — pre-refactor each chassis open-coded the
/// `frame.is_multiple_of(LOG_EVERY_FRAMES)` check.
///
/// Pushes a single 16-byte cast-encoded `FrameStats` to the
/// broadcast mailbox via `queue.push`; observation routing is
/// handled by the registered sink that owns the broadcast name.
/// Fire-and-forget — the broadcast fans out to every attached
/// Claude session, no reply expected. The `tracing::info!` log
/// line is left to the caller because chassis carry chassis-
/// specific context (FPS, elapsed) the helper shouldn't decide
/// the schema for.
///
/// Stage 3 of issue 552 retired the `broadcast_mbox` parameter:
/// the recipient is `crate::HubBroadcast::MAILBOX_ID`, a const-
/// evaluated id matching whatever `register_sink(HubBroadcast::NAMESPACE,
/// ...)` returns at boot. `sender` likewise retired — the broadcast
/// path is target-by-mailbox + fan-out, no reply, so a sender
/// identity wasn't read by any consumer.
pub fn emit_frame_stats(
    queue: &Mailer,
    kind_frame_stats: aether_data::KindId,
    frame: u64,
    triangles: u64,
) {
    if !frame.is_multiple_of(LOG_EVERY_FRAMES) {
        return;
    }
    queue.push(Mail::new(
        crate::HubBroadcast::MAILBOX_ID,
        kind_frame_stats,
        encode(&FrameStats { frame, triangles }),
        1,
    ));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use aether_data::Kind;
    use aether_kinds::FrameStats;

    use super::*;
    use crate::component::Component;
    use crate::ctx::SubstrateCtx;
    use crate::host_fns;
    use crate::input;
    use crate::mail::MailboxId;
    use crate::registry::Registry;
    use crate::scheduler::{ComponentEntry, ComponentTable, DrainDeath, DrainSummary};

    /// Minimal no-op WAT: exports `memory` and a `receive_p32` that
    /// returns 0. Suffices for spawning a `ComponentEntry`; we only
    /// exercise the gate state, never `deliver`.
    const WAT_NOOP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    fn minimal_component() -> Component {
        let engine = wasmtime::Engine::default();
        let mut linker: wasmtime::Linker<SubstrateCtx> = wasmtime::Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(WAT_NOOP).expect("compile WAT");
        let module = wasmtime::Module::new(&engine, &wasm).expect("compile module");
        let ctx = SubstrateCtx::new(
            MailboxId(0),
            Arc::new(Registry::new()),
            Arc::new(Mailer::new()),
            HubOutbound::disconnected(),
            input::new_subscribers(),
        );
        Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate")
    }

    /// `abort_reason` returns `None` for a clean drain summary.
    #[test]
    fn abort_reason_none_for_clean_summary() {
        let summary = DrainSummary::default();
        assert!(abort_reason(&summary).is_none());
    }

    /// `abort_reason` formats wedge messages with Display form for
    /// the mailbox id (post-issue-435) — `mailbox={mailbox}`, not
    /// `mailbox={mailbox:?}`. Pin the format string so a chassis
    /// regression can't drift it back to Debug.
    ///
    /// The wedge path is what `drain_or_abort` would route to
    /// `lifecycle::fatal_abort`; calling that directly would
    /// `std::process::exit` and end the test. Asserting on
    /// `abort_reason` (the same formatter) is the equivalent
    /// assertion without the exit.
    #[test]
    fn abort_reason_formats_wedge_with_display_mailbox() {
        let mailbox = MailboxId(0xDEAD_BEEF_CAFE_F00D_u64);
        let waited = Duration::from_millis(5_000);
        let summary = DrainSummary {
            deaths: Vec::new(),
            wedged: Some((mailbox, waited)),
        };
        let reason = abort_reason(&summary).expect("wedged summary yields a reason");
        let expected = format!("dispatcher wedged: mailbox={mailbox} waited={waited:?}");
        assert_eq!(reason, expected);
        // Guard against accidental `{mailbox:?}` reintroduction —
        // Display for `MailboxId` is `mbx-<hex>`, Debug wraps in the
        // tuple-struct form.
        assert!(
            !reason.contains("MailboxId("),
            "wedge reason must use Display, not Debug, for the mailbox id (got: {reason})",
        );
    }

    /// Wedge takes precedence over deaths when both are present —
    /// matches `drain_or_abort`'s ordering. The reason quotes the
    /// wedge, deaths are logged structurally but don't show in the
    /// reason string.
    #[test]
    fn abort_reason_wedge_wins_over_deaths() {
        let mailbox = MailboxId(0x42);
        let waited = Duration::from_secs(5);
        let summary = DrainSummary {
            deaths: vec![DrainDeath {
                mailbox: MailboxId(0x99),
                mailbox_name: "doomed".into(),
                last_kind: "test.kind".into(),
                reason: "trap".into(),
            }],
            wedged: Some((mailbox, waited)),
        };
        let reason = abort_reason(&summary).expect("non-empty summary");
        assert!(reason.starts_with("dispatcher wedged:"));
        assert!(!reason.contains("component died:"));
    }

    /// `abort_reason` formats death messages with the first death's
    /// mailbox name + last kind + reason. Pins the format string.
    #[test]
    fn abort_reason_formats_first_death() {
        let summary = DrainSummary {
            deaths: vec![DrainDeath {
                mailbox: MailboxId(0x77),
                mailbox_name: "alpha".into(),
                last_kind: "kind.alpha".into(),
                reason: "alpha trap".into(),
            }],
            wedged: None,
        };
        let reason = abort_reason(&summary).expect("death yields a reason");
        assert_eq!(
            reason,
            "component died: alpha (kind kind.alpha) — alpha trap",
        );
    }

    /// End-to-end: a stuck dispatcher (pending bumped without a
    /// matching mail) makes `drain_all_with_budget` return a
    /// Wedged summary, and `abort_reason` produces the expected
    /// reason string. Calling `drain_or_abort` directly would
    /// `std::process::exit` at the end, so the test runs the same
    /// drain the helper runs and the same formatter, just with the
    /// terminal `lifecycle::fatal_abort` step swapped for an
    /// equality assertion.
    ///
    /// Pre-issue-427 the wedge-abort path had zero unit coverage on
    /// any chassis (the chassis frame loops kill the process on
    /// the call). Co-locating it with the helper makes the
    /// regression-detection cost a single `cargo test`.
    #[test]
    fn stuck_dispatcher_drains_to_wedge_and_aborts_with_display_mailbox() {
        let registry = Arc::new(Registry::new());
        let mailbox = registry.register_component("stuck");
        let mailer = Arc::new(Mailer::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            minimal_component(),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox,
        ));
        components
            .write()
            .unwrap()
            .insert(mailbox, Arc::clone(&entry));

        // Bump pending without sending mail: dispatcher never wakes,
        // never decrements. `drain_all_with_budget` must return a
        // Wedged summary at the budget.
        entry.bump_pending_for_test();

        let summary = mailer.drain_all_with_budget(Duration::from_millis(50));
        let (wedge_mailbox, _waited) = summary
            .wedged
            .as_ref()
            .expect("stuck dispatcher must surface as wedged");
        assert_eq!(*wedge_mailbox, mailbox);

        let reason = abort_reason(&summary).expect("wedged summary yields a reason");
        assert!(
            reason.starts_with("dispatcher wedged: mailbox="),
            "reason must lead with `dispatcher wedged: mailbox=` (got: {reason})",
        );
        assert!(reason.contains("waited="));
        // Pin the Display-vs-Debug contract on the mailbox id —
        // the same guard the format-string-only test enforces, but
        // for the live drain path. Display for `MailboxId` is
        // `mbx-<hex>`; Debug wraps in `MailboxId(...)`.
        assert!(
            !reason.contains("MailboxId("),
            "wedge reason must format mailbox via Display, not Debug (got: {reason})",
        );

        // Restore pending so the dispatcher's teardown drain
        // assertions stay clean, then drop entry refs so the
        // dispatcher's mpsc Sender is the last strong ref and the
        // dispatcher thread joins on close.
        entry.clear_pending_for_test();
        drop(entry);
        components.write().unwrap().clear();
    }

    /// `emit_frame_stats` is a no-op on non-multiples of
    /// `LOG_EVERY_FRAMES`. Verified by sending into a sink that
    /// records every payload — a non-multiple frame must produce
    /// zero deliveries.
    #[test]
    fn emit_frame_stats_skips_non_multiples() {
        use aether_actor::Actor;
        let registry = Arc::new(Registry::new());
        let captured: Arc<RwLock<Vec<Vec<u8>>>> = Arc::new(RwLock::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(
                move |_kind_id, _kind_name, _origin, _sender, bytes, _count| {
                    captured_for_sink.write().unwrap().push(bytes.to_vec());
                },
            ),
        );
        let mailer = Arc::new(Mailer::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), components);

        let kind_id = FrameStats::ID;
        emit_frame_stats(&mailer, kind_id, 1, 0);
        emit_frame_stats(&mailer, kind_id, 119, 0);
        assert!(captured.read().unwrap().is_empty());
    }

    /// `emit_frame_stats` emits a FrameStats payload on the
    /// `LOG_EVERY_FRAMES` boundary. The captured bytes round-trip
    /// through the cast decoder back to the input values.
    #[test]
    fn emit_frame_stats_emits_on_multiple() {
        use aether_actor::Actor;
        let registry = Arc::new(Registry::new());
        let captured: Arc<RwLock<Vec<Vec<u8>>>> = Arc::new(RwLock::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(
                move |_kind_id, _kind_name, _origin, _sender, bytes, _count| {
                    captured_for_sink.write().unwrap().push(bytes.to_vec());
                },
            ),
        );
        let mailer = Arc::new(Mailer::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), components);

        emit_frame_stats(&mailer, FrameStats::ID, LOG_EVERY_FRAMES, 42);
        let frames = captured.read().unwrap();
        assert_eq!(frames.len(), 1, "one delivery on the boundary");
        let stats: FrameStats = aether_data::decode(&frames[0]).expect("decode FrameStats");
        assert_eq!(stats.frame, LOG_EVERY_FRAMES);
        assert_eq!(stats.triangles, 42);
    }
}
