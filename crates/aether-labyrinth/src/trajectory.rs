//! `aether.trajectory` cap. Subscribes to a per-tick stream of a moving
//! point's grid position plus a scalar accumulator value, accumulating
//! samples into a typed, seed-keyed `TrajectoryLog` handle (ADR-0049)
//! that an offline analysis transform can replay.
//!
//! State is a plain `sessions` `HashMap` plus an `Arc<HandleStore>` cloned
//! at `init` from `ctx.mailer().handle_store()` — the same pattern as
//! `HandleCapability`. Every handler runs on the cap's dispatcher thread
//! (ADR-0078 plain-field, no locks). Registered as `aether.trajectory`
//! via `with_common_caps` on desktop and headless chassis; the test-bench
//! chassis adds it explicitly.
//!
//! Two handlers:
//!
//! - `on_sample` — fire-and-forget: append `(tick, x, y, value)` to the
//!   buffer for `seed`, creating the buffer on first sample for that seed.
//! - `on_end` — build a `TrajectoryLog { seed, samples, end_reason }`,
//!   postcard-encode it via `encode_into_bytes`, publish it to the handle
//!   store via `next_ephemeral` → `put` → `inc_ref`, drop the in-memory
//!   buffer, and return `RecordResult` (ADR-0112 return-type reply form).

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_kinds::{TrajectoryEnd, TrajectorySample};

#[aether_actor::bridge(singleton)]
mod native {
    use std::collections::HashMap;
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_data::Kind;
    use aether_kinds::{
        RecordResult, TrajectoryEnd, TrajectoryLog, TrajectorySample, TrajectorySampleEntry,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::handle_store::HandleStore;

    /// `aether.trajectory` mailbox cap. Accumulates per-tick samples
    /// from a moving point into a typed `TrajectoryLog` handle (ADR-0049)
    /// that an offline analysis transform can replay.
    ///
    /// State is plain fields — single-threaded, every handler on the
    /// cap's dispatcher thread (ADR-0078). No locks needed.
    pub struct TrajectoryRecorderCapability {
        /// Per-seed in-flight sample buffers. Created on the first
        /// `TrajectorySample` for a seed; flushed and removed on the
        /// matching `TrajectoryEnd`.
        sessions: HashMap<u64, Vec<TrajectorySampleEntry>>,
        /// Shared handle store — cloned from the substrate's mailer at
        /// `init`, the same as `HandleCapability`.
        store: Arc<HandleStore>,
    }

    #[actor]
    impl NativeActor for TrajectoryRecorderCapability {
        type Config = ();

        /// ADR-0074 §5: chassis-owned mailbox under the `aether.<name>`
        /// namespace. Single-segment name matches the dominant chassis-cap
        /// convention (`aether.input`, `aether.render`, `aether.handle`).
        const NAMESPACE: &'static str = "aether.trajectory";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let store = Arc::clone(ctx.mailer().handle_store());
            Ok(Self {
                sessions: HashMap::new(),
                store,
            })
        }

        /// Append one per-tick sample to the in-flight buffer for
        /// `s.seed`. Creates the buffer on first arrival for that seed.
        /// Fire-and-forget — no reply.
        ///
        /// # Agent
        /// No reply (fire-and-forget). Address this cap by type:
        /// `ctx.actor::<TrajectoryRecorderCapability>().send(&sample)`.
        #[handler]
        fn on_sample(&mut self, _ctx: &mut NativeCtx<'_>, s: TrajectorySample) {
            self.sessions
                .entry(s.seed)
                .or_default()
                .push(TrajectorySampleEntry {
                    tick: s.tick,
                    x: s.x,
                    y: s.y,
                    value: s.value,
                });
        }

        /// Flush the in-flight buffer for `e.seed`, build a
        /// `TrajectoryLog`, publish it to the handle store, and return
        /// `RecordResult`.
        ///
        /// # Agent
        /// Reply: `RecordResult`. `Ok` carries the seed, the minted
        /// `handle_id`, and `kind_id` (`TrajectoryLog::ID`). Use
        /// `handle_id` to pass the log to a DAG transform or retrieve it
        /// later via `describe_handles`. `Err` when `seed` names no
        /// in-flight session (unknown seed or already terminated).
        #[handler]
        fn on_end(&mut self, _ctx: &mut NativeCtx<'_>, e: TrajectoryEnd) -> RecordResult {
            let Some(samples) = self.sessions.remove(&e.seed) else {
                return RecordResult::Err {
                    seed: e.seed,
                    error: format!("no in-flight session for seed {}", e.seed),
                };
            };

            let log = TrajectoryLog {
                seed: e.seed,
                samples,
                end_reason: e.reason,
            };

            let id = self.store.next_ephemeral();
            let bytes = log.encode_into_bytes();
            let kind_id = TrajectoryLog::ID;

            match self.store.put(id, kind_id, bytes) {
                Ok(()) => {
                    // Hold a reference on behalf of the recorder,
                    // mirroring `HandleCapability::on_publish`.
                    self.store.inc_ref(id);
                    RecordResult::Ok {
                        seed: e.seed,
                        handle_id: id,
                        kind_id,
                    }
                }
                Err(err) => RecordResult::Err {
                    seed: e.seed,
                    error: format!("handle store put failed: {err:?}"),
                },
            }
        }
    }

    #[cfg(test)]
    #[allow(
        clippy::unwrap_used,
        reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
    )]
    mod tests {
        use std::sync::mpsc;

        use aether_data::{HandleId, Kind, MailId, MailboxId};
        use aether_kinds::TrajectoryEndReason;
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::{OwnedDispatch, Registry};
        use aether_substrate::mail::{MailRef, Source, SourceAddr};

        use super::*;
        use crate::test_chassis::{TestChassis, boot_test_chassis_with};
        use aether_actor::Actor;
        use aether_substrate::chassis::builder::Builder;

        /// Build a substrate where the `Arc<HandleStore>` is also returned
        /// to the caller. Used by both the direct-call unit tests and the
        /// heavy dispatcher test.
        fn fresh_substrate() -> (
            Arc<HandleStore>,
            Arc<Mailer>,
            Arc<Registry>,
            mpsc::Receiver<EgressEvent>,
        ) {
            let store = Arc::new(HandleStore::new(64 * 1024));
            let registry = Arc::new(Registry::new());
            for d in descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            (store, mailer, registry, rx)
        }

        /// Create a session-targeted `Source` for direct handler calls.
        fn session_source() -> Source {
            use aether_data::{SessionToken, Uuid};
            Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(
                0xfeed_u128,
            ))))
        }

        /// Directly-exercised capability fixture (no dispatcher thread).
        struct DirectFixture {
            cap: TrajectoryRecorderCapability,
            store: Arc<HandleStore>,
            transport: Arc<NativeBinding>,
        }

        fn direct_fixture() -> DirectFixture {
            let (store, mailer, _registry, _rx) = fresh_substrate();
            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&mailer),
                MailboxId(0x1862),
            ));
            let cap = TrajectoryRecorderCapability {
                sessions: HashMap::new(),
                store: Arc::clone(&store),
            };
            DirectFixture {
                cap,
                store,
                transport,
            }
        }

        fn make_ctx(transport: &Arc<NativeBinding>) -> NativeCtx<'_> {
            NativeCtx::new(transport, session_source(), MailId::NONE, MailId::NONE)
        }

        // Unit tests — direct handler calls, no dispatcher thread.

        /// A sample for a fresh seed creates a buffer and appends the
        /// entry. A second sample for the same seed appends to the same
        /// buffer.
        #[test]
        fn on_sample_appends_to_seed_buffer() {
            let mut fix = direct_fixture();
            let mut ctx = make_ctx(&fix.transport);

            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed: 42,
                    tick: 1,
                    x: 3,
                    y: 4,
                    value: 99,
                },
            );
            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed: 42,
                    tick: 2,
                    x: 5,
                    y: 6,
                    value: 100,
                },
            );

            let buf = fix.cap.sessions.get(&42).expect("seed 42 buffer exists");
            assert_eq!(buf.len(), 2, "two samples for seed 42");
            assert_eq!(
                buf[0],
                TrajectorySampleEntry {
                    tick: 1,
                    x: 3,
                    y: 4,
                    value: 99
                }
            );
            assert_eq!(
                buf[1],
                TrajectorySampleEntry {
                    tick: 2,
                    x: 5,
                    y: 6,
                    value: 100
                }
            );
        }

        /// Two interleaved seeds accumulate into separate buffers; neither
        /// spills into the other.
        #[test]
        fn on_sample_keeps_seeds_separate() {
            let mut fix = direct_fixture();
            let mut ctx = make_ctx(&fix.transport);

            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed: 1,
                    tick: 10,
                    x: 0,
                    y: 0,
                    value: 1,
                },
            );
            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed: 2,
                    tick: 11,
                    x: 7,
                    y: 8,
                    value: 2,
                },
            );
            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed: 1,
                    tick: 12,
                    x: 1,
                    y: 0,
                    value: 3,
                },
            );

            let buf1 = fix.cap.sessions.get(&1).expect("seed 1 buffer exists");
            let buf2 = fix.cap.sessions.get(&2).expect("seed 2 buffer exists");
            assert_eq!(buf1.len(), 2, "seed 1 has two samples");
            assert_eq!(buf2.len(), 1, "seed 2 has one sample");
            assert_eq!(
                buf2[0],
                TrajectorySampleEntry {
                    tick: 11,
                    x: 7,
                    y: 8,
                    value: 2
                }
            );
        }

        /// `on_end` publishes a handle containing a `TrajectoryLog` that
        /// decodes back to the samples in tick order, removes the buffer,
        /// and returns `RecordResult::Ok`.
        #[test]
        fn on_end_publishes_decodable_log() {
            let mut fix = direct_fixture();
            let mut ctx = make_ctx(&fix.transport);

            let seed = 7u64;
            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed,
                    tick: 1,
                    x: 0,
                    y: 0,
                    value: 10,
                },
            );
            fix.cap.on_sample(
                &mut ctx,
                TrajectorySample {
                    seed,
                    tick: 2,
                    x: 1,
                    y: 0,
                    value: 20,
                },
            );

            let result = fix.cap.on_end(
                &mut ctx,
                TrajectoryEnd {
                    seed,
                    reason: TrajectoryEndReason::Completed,
                },
            );

            let RecordResult::Ok {
                seed: out_seed,
                handle_id,
                kind_id,
            } = result
            else {
                panic!("expected RecordResult::Ok, got {result:?}");
            };

            assert_eq!(out_seed, seed, "seed echoed");
            assert_ne!(handle_id, HandleId(0), "handle id is non-zero");
            assert_eq!(kind_id, TrajectoryLog::ID, "kind id matches TrajectoryLog");

            // Verify the session buffer was cleared.
            assert!(
                !fix.cap.sessions.contains_key(&seed),
                "session buffer removed after on_end"
            );

            // Verify the stored bytes decode back to the expected log.
            let (stored_kind, stored_bytes) =
                fix.store.get(handle_id).expect("handle exists in store");
            assert_eq!(stored_kind, TrajectoryLog::ID, "stored kind id matches");
            let log = TrajectoryLog::decode_from_bytes(&stored_bytes)
                .expect("stored bytes decode to TrajectoryLog");
            assert_eq!(log.seed, seed);
            assert_eq!(log.end_reason, TrajectoryEndReason::Completed);
            assert_eq!(log.samples.len(), 2);
            assert_eq!(
                log.samples[0],
                TrajectorySampleEntry {
                    tick: 1,
                    x: 0,
                    y: 0,
                    value: 10
                }
            );
            assert_eq!(
                log.samples[1],
                TrajectorySampleEntry {
                    tick: 2,
                    x: 1,
                    y: 0,
                    value: 20
                }
            );
        }

        /// `on_end` for a seed with no in-flight session returns
        /// `RecordResult::Err`.
        #[test]
        fn on_end_unknown_seed_returns_err() {
            let mut fix = direct_fixture();
            let mut ctx = make_ctx(&fix.transport);

            let result = fix.cap.on_end(
                &mut ctx,
                TrajectoryEnd {
                    seed: 999,
                    reason: TrajectoryEndReason::Aborted,
                },
            );

            assert!(
                matches!(result, RecordResult::Err { seed: 999, .. }),
                "expected Err for unknown seed, got {result:?}"
            );
        }

        /// End-to-end through the dispatcher thread: boot the cap via
        /// `boot_test_chassis_with`, enqueue `TrajectorySample`s then a
        /// `TrajectoryEnd`, assert `RecordResult::Ok` arrives on the
        /// loopback channel, and verify the store holds a decodable
        /// `TrajectoryLog`. Sleep-polls under a multi-second deadline →
        /// `mod heavy` for the `serial-heavy` nextest group (issue 1522).
        mod heavy {
            use std::thread;
            use std::time::{Duration, Instant};

            use aether_data::{SessionToken, Uuid};
            use aether_substrate::mail::registry::MailboxEntry;

            use super::*;

            #[test]
            fn capability_routes_end_through_dispatcher_thread() {
                let (store, mailer, registry, rx) = fresh_substrate();

                let chassis =
                    boot_test_chassis_with::<TrajectoryRecorderCapability>(&registry, &mailer, ());

                let mbx_id = registry
                    .lookup(TrajectoryRecorderCapability::NAMESPACE)
                    .expect("aether.trajectory registered");

                let MailboxEntry::Inbox { handler, .. } =
                    registry.entry(mbx_id).expect("entry exists")
                else {
                    panic!("expected Inbox entry");
                };

                let seed = 1234u64;

                // Send two samples.
                for (tick, x, y, value) in [(1u32, 2u32, 3u32, 10u32), (2, 4, 5, 20)] {
                    let s = TrajectorySample {
                        seed,
                        tick,
                        x,
                        y,
                        value,
                    };
                    let bytes = s.encode_into_bytes();
                    handler.enqueue(OwnedDispatch::disarmed(
                        <TrajectorySample as Kind>::ID,
                        "aether.trajectory.sample".to_owned(),
                        None,
                        Source::NONE,
                        MailRef::from(bytes),
                        1,
                        MailId::NONE,
                        MailId::NONE,
                        None,
                        Nanos(0),
                        0,
                        MailboxId(0),
                    ));
                }

                // Send the terminal event with a session reply target so
                // the dispatcher can route the RecordResult reply.
                let reply_to =
                    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0x1862))));
                let end = TrajectoryEnd {
                    seed,
                    reason: TrajectoryEndReason::Completed,
                };
                let bytes = end.encode_into_bytes();
                handler.enqueue(OwnedDispatch::disarmed(
                    <TrajectoryEnd as Kind>::ID,
                    "aether.trajectory.end".to_owned(),
                    None,
                    reply_to,
                    MailRef::from(bytes),
                    1,
                    MailId::NONE,
                    MailId::NONE,
                    None,
                    Nanos(0),
                    0,
                    MailboxId(0),
                ));

                // Poll the outbound channel for the RecordResult reply.
                let deadline = Instant::now() + Duration::from_secs(5);
                let payload = loop {
                    if let Ok(EgressEvent::ToSession { payload, .. }) = rx.try_recv() {
                        break payload;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "RecordResult reply did not arrive within deadline"
                    );
                    thread::sleep(Duration::from_millis(5));
                };

                let result =
                    RecordResult::decode_from_bytes(&payload).expect("RecordResult decodes");
                let RecordResult::Ok {
                    seed: out_seed,
                    handle_id,
                    kind_id,
                } = result
                else {
                    panic!("expected RecordResult::Ok, got {result:?}");
                };

                assert_eq!(out_seed, seed, "seed echoed");
                assert_ne!(handle_id, HandleId(0), "handle id is non-zero");
                assert_eq!(kind_id, TrajectoryLog::ID, "kind id matches TrajectoryLog");

                // Verify the handle store holds a decodable log.
                let (stored_kind, stored_bytes) =
                    store.get(handle_id).expect("handle exists in store");
                assert_eq!(stored_kind, TrajectoryLog::ID);
                let log = TrajectoryLog::decode_from_bytes(&stored_bytes)
                    .expect("stored bytes decode to TrajectoryLog");
                assert_eq!(log.seed, seed);
                assert_eq!(log.end_reason, TrajectoryEndReason::Completed);
                assert_eq!(log.samples.len(), 2);

                drop(chassis);
            }
        }

        /// Builder rejects a duplicate claim on `aether.trajectory`.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            use aether_substrate::chassis::error::BootError;
            use aether_substrate::mail::registry;

            let (_store, mailer, registry_arc, _rx) = fresh_substrate();
            registry_arc.register_inbox(
                TrajectoryRecorderCapability::NAMESPACE,
                registry::noop_handler(),
            );

            let err = Builder::<TestChassis>::new(Arc::clone(&registry_arc), Arc::clone(&mailer))
                .with_actor::<TrajectoryRecorderCapability>(())
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == TrajectoryRecorderCapability::NAMESPACE
            ));
        }
    }
}
