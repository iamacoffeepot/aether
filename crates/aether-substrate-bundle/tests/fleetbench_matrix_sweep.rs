//! `FleetBench` cluster-addressing matrix sweep (issue 1977, ADR-0114
//! amendment): load the `matrix_sweep` cluster fixture + a cross-cluster
//! `source_observer`, drive the sweep over the real `WireFrame::Call` wire,
//! read back the structured `MatrixReport`, and assert every cell — delivery
//! AND the source the recipient read (`ctx.source_mailbox()`).
//!
//! Cells asserted from the report (in-cluster, in-place dispatch):
//!
//! - parent → child[a]: child[a] received it; its source is the parent id.
//! - child[a] → parent: the parent received it; its source is child[a]'s id
//!   (the in-place "from" half — Task 1).
//! - child[a] → sibling child[b]: child[b] received it; its source is
//!   child[a]'s id.
//! - child[a] → self: child[a] re-received it; its source is its own id.
//!
//! Cell asserted out-of-band (cross-cluster, during the in-place drain — the
//! Task 2 documented boundary): the `source_observer`, mailed by child[a]
//! during the `RunMatrix` drain, logs the source it read. Per Task 2 that is
//! the cluster's *inbound* dispatch identity (the parent, the recipient of
//! the wire `RunMatrix` call), NOT child[a]'s id — the drain runs inside one
//! `receive_p32` and never updates the host-side dispatch identity. The test
//! asserts that documented behavior; it does not fail on it.
//!
//! What this layer proves vs. the unit tests: `FleetBench` proves to-and-from
//! delivery and the source the recipient reads, end-to-end over the real RPC
//! stack. The in-place *mechanism* (whether a send ran in place vs. via the
//! scheduler) is not externally observable over the wire — that is covered by
//! the Task 1 unit tests in `aether-actor` (`drained_child_reads_*`). The
//! cells here distinguish the directions and the resolved sources, which is
//! what the wire layer can witness.
//!
//! Heavy by construction (fork+exec + cross-process settle) — the test lives
//! in `mod tests::heavy` so nextest's `test(/::heavy::/)` selector serializes
//! it in the `serial-heavy` group.

mod fleetbench;

mod tests {
    mod heavy {
        use aether_data::Kind;
        use aether_kinds::{LogEntry, LogTailResult};
        use aether_test_fixtures::{CollectMatrix, MatrixReport, RunMatrix};

        use crate::fleetbench::{FleetBench, dist_manifest_present};

        /// Drive the full cluster-addressing matrix over the wire and assert
        /// every cell: in-cluster delivery + the source each recipient read,
        /// plus the cross-cluster boundary cell observed via the observer's
        /// log.
        #[test]
        fn fleetbench_matrix_sweep_covers_every_addressing_cell() {
            if !dist_manifest_present() {
                return;
            }
            let mut bench = FleetBench::start();
            let engine = bench.spawn_headless();

            // The cluster (parent + two inline children) and a separate
            // cross-cluster observer component.
            let parent_addr = bench.load(engine, "matrix_sweep");
            let observer = bench.load_full(engine, "source_observer");

            // Drive the sweep: the parent fans out every in-cluster direction
            // in place, plus a cross-cluster send to the observer during the
            // drain. The whole cascade settles before this `send` returns.
            let run_replies = bench.send(
                engine,
                &parent_addr,
                &RunMatrix {
                    observer_mailbox: observer.mailbox_id.0,
                },
            );
            assert!(
                run_replies.is_empty(),
                "RunMatrix is fire-and-settle (no reply), got {} reply events",
                run_replies.len(),
            );

            // Read the cluster's recorded observations.
            let report_replies = bench.send(engine, &parent_addr, &CollectMatrix);
            let report_env = match report_replies.as_slice() {
                [one] => one,
                other => panic!(
                    "CollectMatrix should reply exactly one MatrixReport, got {}",
                    other.len(),
                ),
            };
            assert_eq!(
                report_env.kind,
                MatrixReport::ID,
                "the CollectMatrix reply should be a MatrixReport",
            );
            let report = MatrixReport::decode_from_bytes(&report_env.payload)
                .expect("the reply decodes as MatrixReport");

            let parent_id = report.parent_id;
            let child_a_id = report.child_a_id;
            assert_ne!(parent_id, 0, "the parent recorded its own id");
            assert_ne!(child_a_id, 0, "the parent recorded child[a]'s id");
            assert_ne!(
                parent_id, child_a_id,
                "the parent and child[a] are distinct addresses",
            );

            // Cell: parent -> child[a] (in place). child[a] received it and
            // read the parent's id as its source.
            assert_eq!(
                report.parent_to_child_arrived, 1,
                "parent -> child[a] should be delivered",
            );
            assert_eq!(
                report.parent_to_child_source, parent_id,
                "child[a] should read the parent's id as the source of parent -> child[a]",
            );

            // Cell: child[a] -> parent (in place). The parent received it and
            // read child[a]'s id as its source (the Task 1 in-place "from").
            assert_eq!(
                report.child_to_parent_arrived, 1,
                "child[a] -> parent should be delivered",
            );
            assert_eq!(
                report.child_to_parent_source, child_a_id,
                "the parent should read child[a]'s id as the source of child[a] -> parent",
            );

            // Cell: child[a] -> sibling child[b] (in place). child[b] received
            // it and read child[a]'s id as its source.
            assert_eq!(
                report.child_to_sibling_arrived, 1,
                "child[a] -> sibling child[b] should be delivered",
            );
            assert_eq!(
                report.child_to_sibling_source, child_a_id,
                "child[b] should read child[a]'s id as the source of child[a] -> sibling",
            );

            // Cell: child[a] -> self (in place). child[a] re-received it and
            // read its own id as its source.
            assert_eq!(
                report.child_to_self_arrived, 1,
                "child[a] -> self should be delivered",
            );
            assert_eq!(
                report.child_to_self_source, child_a_id,
                "child[a] should read its own id as the source of child[a] -> self",
            );

            // Cross-cluster cell (the Task 2 documented boundary): the
            // observer, mailed by child[a] during the RunMatrix drain, logged
            // the source it read. The drain runs inside the parent's one
            // `receive_p32`, so the host stamps the cluster's inbound
            // identity (the parent) as the origin — NOT child[a]. Assert the
            // documented behavior.
            let expected = format!("source_mailbox={parent_id}");
            let entries = match bench.log_tail(engine, &observer.addr, None) {
                LogTailResult::Ok { entries, .. } => entries,
                LogTailResult::Err { error } => panic!("log_tail on observer failed: {error}"),
            };
            let logged: Vec<&LogEntry> = entries
                .iter()
                .filter(|e| e.message.starts_with("source_mailbox="))
                .collect();
            assert!(
                logged.iter().any(|e| e.message == expected),
                "the cross-cluster observer should log the cluster's inbound identity \
                 (parent id {parent_id}) as the source of a send made during the in-place \
                 drain (Task 2 boundary), not child[a]'s id {child_a_id};\n\
                 expected message: {expected:?}\n\
                 logged source_mailbox entries: {logged:?}",
            );
        }
    }
}
