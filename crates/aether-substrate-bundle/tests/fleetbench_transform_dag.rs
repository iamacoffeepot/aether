//! `FleetBench` Transform-node DAG proof (issue 1472): the wire-driven
//! `Source → Transform → Observer` computation DAG (ADR-0047 / ADR-0048)
//! over the real hub → RPC → forked-headless-substrate stack. Where
//! `fleetbench_dag.rs` (#1461) covers only terminal effectful `Source`
//! nodes, this drives the production `Transform`-node dispatch path: a
//! first-party `mat4_source` fixture replies the transform's `Mat4Apply`
//! input, the linked `mat4_apply` transform computes `M·v`, and a
//! `vec4_observer` fixture resolves the transform's `Ref<Vec4>` output
//! and surfaces the value via `aether.fs.write` so the test can read it
//! back and assert exact equality.
//!
//! This is also the only end-to-end check of the cast `Mat4Apply` /
//! `Vec4` wire round-trip across the source-reply → transform-input and
//! transform-output → observer-slot boundaries on the real wire.
//!
//! Heavy by construction (fork+exec + component load + cross-process
//! settle) — the test lives in `mod tests::heavy` so nextest's
//! `test(/::heavy::/)` selector serializes it in the `serial-heavy`
//! group.

mod fleetbench;

// Force-link `aether-labyrinth` into this test binary so its certifier
// `#[transform]`s reach the link-time inventory the guard test below reads
// (issue 1908) — `extern crate as _` in the bundle lib does not propagate
// the whole crate into a downstream binary that links the lib rlib.
extern crate aether_labyrinth as _;

mod tests {
    /// The headless chassis builds its DAG `TransformRegistry` from the
    /// link-time `aether_data::transforms()` inventory, so the reachability
    /// certifier transforms must be linked into this binary — they reach it
    /// only via the bundle's `aether-labyrinth` dependency edge (issue
    /// 1908). Not heavy (a pure local-inventory read, no fork), so it runs
    /// in the fast set and guards the edge: drop the dep and the certifier
    /// transforms silently vanish from the registry with no compile error.
    #[test]
    fn certifier_transforms_registered_in_bundle_inventory() {
        use aether_data::transforms;
        assert!(
            transforms().any(|t| t.name.ends_with("::solve")),
            "the aether-labyrinth `solve` transform must be in the bundle's \
             link-time inventory; a dropped dependency edge silently de-registers it",
        );
    }

    mod heavy {
        use std::thread;
        use std::time::{Duration, Instant};

        use aether_data::{EngineId, Kind, transforms};
        use aether_kinds::{
            DagDescriptor, Edge, Node, NodeId, Read, ReadResult, StatusResult, SubmitResult,
        };
        use aether_math::Vec4;
        use aether_test_fixtures::{Mat4SourceTrigger, Vec4Observed};

        use crate::fleetbench::{FleetBench, dist_manifest_present};

        /// The observer's surfaced output file, under the `save`
        /// namespace. The `vec4_observer` fixture writes the resolved
        /// `Vec4`'s 16 cast bytes here on its `Vec4Observed` handler.
        const OUTPUT_PATH: &str = "dag-vec4-output.bin";

        /// Load both fixtures, submit a `Source → Transform → Observer`
        /// DAG against a forked `aether-substrate-headless`, poll to
        /// `Complete`, read back the observer's surfaced `Vec4`, and
        /// assert it equals the hand-computed `M·v = (7,9,11,1)`. Proves
        /// the production Transform-node dispatch path plus the cast
        /// `Mat4Apply` / `Vec4` round-trip across the real wire.
        #[test]
        fn fleetbench_transform_dag_observer_sees_m_times_v() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();

            // Load both fixtures. `LoadResult.mailbox_id` is the ADR-0099
            // lineage-fold id the load actually registered (the id is the
            // lineage fold, not `hash(name)`), so the descriptor's
            // `Source.mailbox` / `Observer.recipient` route to exactly the
            // mailboxes the components occupy.
            let source = bench.load_full(engine, "mat4_source");
            let observer = bench.load_full(engine, "vec4_observer");

            // Resolve `mat4_apply`'s TransformId from the link-time
            // inventory linked into this test binary (via the bundle's
            // `aether-capabilities` dep). The forked headless registers
            // the same transform from the same inventory, so the ids
            // match across the process boundary.
            let transform_id = transforms()
                .find(|t| t.name.ends_with("::mat4_apply"))
                .expect("mat4_apply is registered in the link-time transform inventory")
                .transform_id;

            let descriptor = DagDescriptor {
                version: 1,
                nodes: vec![
                    Node::Source {
                        id: NodeId(0),
                        mailbox: source.mailbox_id,
                        kind_id: Mat4SourceTrigger::ID,
                        payload: Mat4SourceTrigger.encode_into_bytes(),
                    },
                    Node::Transform {
                        id: NodeId(1),
                        transform_id,
                        output_kind_id: Vec4::ID,
                        timeout_ms: None,
                    },
                    Node::Observer {
                        id: NodeId(2),
                        recipient: observer.mailbox_id,
                        kind_id: Vec4Observed::ID,
                    },
                ],
                edges: vec![
                    Edge {
                        from: NodeId(0),
                        to: NodeId(1),
                        slot: 0,
                    },
                    Edge {
                        from: NodeId(1),
                        to: NodeId(2),
                        slot: 0,
                    },
                ],
            };

            // Validation passing (the `Ok` arm) is the core proof that a
            // wire-driven `Source → Transform → Observer` DAG builds
            // against the forked production binary. The DAG assigns one
            // output handle per node, so the transform node (`NodeId(1)`)
            // — which produces the resolved `Vec4` — carries the M·v
            // handle.
            let (dag_id, output_handles) = match bench.submit_dag(engine, &descriptor) {
                SubmitResult::Ok {
                    dag_id,
                    output_handles,
                } => (dag_id, output_handles),
                SubmitResult::Err { error } => {
                    panic!("submit of the Source→Transform→Observer DAG failed: {error:?}")
                }
            };
            assert!(
                output_handles.iter().any(|h| h.node_id == NodeId(1)),
                "the transform node (NodeId(1)) producing M·v is assigned an output handle, \
                 got {output_handles:?}",
            );

            let status = bench.poll_dag(engine, dag_id, Duration::from_secs(30));
            assert!(
                matches!(status, StatusResult::Complete { .. }),
                "the Source→Transform→Observer DAG reaches Complete, got {status:?}",
            );

            // The observer's `aether.fs.write` is fire-and-forget and
            // lands shortly after the DAG reports Complete, so the read is
            // bounded-retried until the file is present — absorbing the
            // async gap between DAG-Complete and the write settling.
            let bytes = read_with_retry(&mut bench, engine, OUTPUT_PATH, Duration::from_secs(30));
            assert_eq!(
                bytes.len(),
                16,
                "the observer surfaced one Vec4 (16 cast bytes), got {} bytes",
                bytes.len(),
            );
            let m_times_v = bytemuck::pod_read_unaligned::<Vec4>(&bytes);
            assert_eq!(
                m_times_v,
                Vec4::new(7.0, 9.0, 11.0, 1.0),
                "the observer surfaced M·v = (7,9,11,1)",
            );
        }

        /// Read `save://<path>` over the wire, retrying until the file
        /// resolves `ReadResult::Ok` or `deadline` elapses. The observer
        /// writes the file as a fire-and-forget side effect after the DAG
        /// completes, so the first read can race ahead of the write.
        fn read_with_retry(
            bench: &mut FleetBench,
            engine: EngineId,
            path: &str,
            deadline: Duration,
        ) -> Vec<u8> {
            let start = Instant::now();
            loop {
                let replies = bench.send::<Read>(
                    engine,
                    "aether.fs",
                    &Read {
                        namespace: "save".to_owned(),
                        path: path.to_owned(),
                    },
                );
                let payload = replies
                    .first()
                    .expect("aether.fs.read yields exactly one reply event")
                    .payload
                    .clone();
                match ReadResult::decode_from_bytes(&payload).expect("decodable ReadResult reply") {
                    ReadResult::Ok { bytes, .. } => return bytes,
                    ReadResult::Err { error, .. } => {
                        assert!(
                            start.elapsed() < deadline,
                            "the observer's output file save://{path} never appeared within \
                             {deadline:?}; last read error: {error:?}",
                        );
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }
}
