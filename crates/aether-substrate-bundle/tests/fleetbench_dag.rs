//! `FleetBench` computation-DAG proofs (issue 1461, Tier-A): the
//! `submit_dag` / `dag_status` / `dag_cancel` (ADR-0047) and
//! `describe_handles` (ADR-0049) rows over the real hub → RPC →
//! forked-headless-substrate stack. They share DAG-descriptor
//! construction + the bounded `poll_dag` loop. Each DAG here is a single
//! terminal `Node::Source` (no edges, no transform): a terminal source
//! is assigned one output handle, dispatches its opaque payload to a real
//! chassis cap, and the cap's reply resolves that handle — reaching
//! `Complete` and making the handle visible to `describe_handles`. A
//! wire-drivable `Source → Transform → Observer` DAG needs first-party
//! fixture components and is deferred to #1472.
//!
//! - **happy path** — a single source targeting `aether.fs` with
//!   `List { save, "" }` (side-effect-free, always replies `Ok`):
//!   `submit_dag` → `Ok` with one output handle, `poll_dag` →
//!   `Complete`, `describe_handles` → the produced handle is in the
//!   store;
//! - **bad descriptor** — two sources sharing `NodeId(0)` reject
//!   synchronously as `DagError::DuplicateNodeId(NodeId(0))`, a pure
//!   structural reject independent of which caps are registered;
//! - **in-flight cancel** — a source targeting the headless
//!   `aether.render` nop with `DrawTriangle` (accepted but never
//!   replied) keeps the DAG deterministically in-flight, so `dag_cancel`
//!   returns `cancelled: true` (and a second cancel `cancelled: false`).
//!
//! Heavy by construction (fork+exec + cross-process settle) — the tests
//! live in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector
//! serializes them in the `serial-heavy` group (and soaks them).

mod fleetbench;

mod tests {
    mod heavy {
        use std::time::Duration;

        use aether_data::{Kind, mailbox_id_from_name};
        use aether_kinds::{
            CancelResult, DagDescriptor, DagError, DrawTriangle, List, Node, NodeId, StatusResult,
            SubmitResult, Vertex,
        };

        use crate::fleetbench::FleetBench;

        /// A single-terminal-`Source` DAG dispatching `aether.fs.list
        /// { save, "" }` to `aether.fs`. The list is side-effect-free and
        /// always replies `ListResult::Ok` (the save root is created at
        /// `FsCapability::init`), so the source resolves and the DAG
        /// completes deterministically.
        fn fs_list_source_dag() -> DagDescriptor {
            DagDescriptor {
                version: 1,
                nodes: vec![fs_list_source(0)],
                edges: vec![],
            }
        }

        /// Two `aether.fs.list` sources sharing `NodeId(0)` — a Phase-1
        /// structural reject (`DuplicateNodeId`) the validator catches
        /// before touching cap/kind state.
        fn duplicate_node_dag() -> DagDescriptor {
            DagDescriptor {
                version: 1,
                nodes: vec![fs_list_source(0), fs_list_source(0)],
                edges: vec![],
            }
        }

        /// One `aether.fs.list` source node with the given descriptor-local
        /// id. The payload is the same inline-encoded `List` regardless of
        /// id, so the duplicate-id descriptor differs only in the colliding
        /// `NodeId`.
        fn fs_list_source(id: u32) -> Node {
            Node::Source {
                id: NodeId(id),
                mailbox: mailbox_id_from_name("aether.fs"),
                kind_id: List::ID,
                payload: List {
                    namespace: "save".to_owned(),
                    prefix: String::new(),
                }
                .encode_into_bytes(),
            }
        }

        /// A single-terminal-`Source` DAG dispatching a `DrawTriangle` to
        /// the headless `aether.render` nop. The nop accepts the kind (so
        /// validation passes) but never replies, so the source handle
        /// never resolves and the DAG stays deterministically in-flight
        /// until cancelled.
        fn render_draw_source_dag() -> DagDescriptor {
            DagDescriptor {
                version: 1,
                nodes: vec![Node::Source {
                    id: NodeId(0),
                    mailbox: mailbox_id_from_name("aether.render"),
                    kind_id: DrawTriangle::ID,
                    payload: DrawTriangle {
                        verts: [Vertex::default(); 3],
                    }
                    .encode_into_bytes(),
                }],
                edges: vec![],
            }
        }

        /// Happy path (matrix rows 1 + 2): submit a single-source DAG,
        /// assert `SubmitResult::Ok` with one output handle, poll to
        /// `Complete`, and assert `describe_handles` surfaces the produced
        /// handle. Proves `submit_dag` (sync validate + async exec) →
        /// `dag_status` poll-to-`Complete` → `describe_handles` over the
        /// real wire path.
        #[test]
        fn fleetbench_dag_submit_completes_and_produces_handle() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();

            let descriptor = fs_list_source_dag();
            let (dag_id, output_handles) = match bench.submit_dag(engine, &descriptor) {
                SubmitResult::Ok {
                    dag_id,
                    output_handles,
                } => (dag_id, output_handles),
                SubmitResult::Err { error } => {
                    panic!("submit of a valid single-source DAG failed: {error:?}")
                }
            };
            assert_eq!(
                output_handles.len(),
                1,
                "a single terminal Source is assigned exactly one output handle",
            );
            let handle_id = output_handles[0].handle_id;

            let status = bench.poll_dag(engine, dag_id, Duration::from_secs(30));
            assert!(
                matches!(status, StatusResult::Complete { .. }),
                "the fs-list source DAG reaches Complete, got {status:?}",
            );

            let summary = bench.describe_handles(engine, 16);
            assert!(
                summary.total_entries >= 1,
                "the completed DAG leaves at least one resolved handle in the store, got {}",
                summary.total_entries,
            );
            assert!(
                summary
                    .top_by_recency
                    .iter()
                    .any(|handle| handle.handle_id == handle_id),
                "the source's output handle {handle_id:?} appears in top_by_recency",
            );
        }

        /// Synchronous bad-descriptor reject (matrix row 1): two sources
        /// sharing `NodeId(0)` reject on the submit call with
        /// `DagError::DuplicateNodeId(NodeId(0))` — validation runs
        /// synchronously, no `dag_id` is minted, nothing dispatches.
        #[test]
        fn fleetbench_dag_rejects_bad_descriptor() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();

            let descriptor = duplicate_node_dag();
            let submit = bench.submit_dag(engine, &descriptor);
            assert!(
                matches!(
                    submit,
                    SubmitResult::Err {
                        error: DagError::DuplicateNodeId(NodeId(0)),
                    },
                ),
                "two sources sharing NodeId(0) reject as DuplicateNodeId(0), got {submit:?}",
            );
        }

        /// In-flight cancel (matrix row 1): a source targeting the
        /// never-replying `aether.render` nop keeps the DAG in-flight, so
        /// `dag_cancel` returns `cancelled: true`. A second cancel of the
        /// now-cancelled DAG returns `cancelled: false` — the false arm,
        /// nothing left to cancel.
        #[test]
        fn fleetbench_dag_cancels_in_flight() {
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();

            let descriptor = render_draw_source_dag();
            let dag_id = match bench.submit_dag(engine, &descriptor) {
                SubmitResult::Ok { dag_id, .. } => dag_id,
                SubmitResult::Err { error } => {
                    panic!("submit of the in-flight render DAG failed: {error:?}")
                }
            };

            let cancel = bench.dag_cancel(engine, dag_id);
            assert!(
                matches!(cancel, CancelResult::Ok { cancelled: true }),
                "cancelling the never-replying render source DAG returns cancelled: true, \
                 got {cancel:?}",
            );

            let recancel = bench.dag_cancel(engine, dag_id);
            assert!(
                matches!(recancel, CancelResult::Ok { cancelled: false }),
                "a second cancel of an already-cancelled DAG returns cancelled: false, \
                 got {recancel:?}",
            );
        }
    }
}
