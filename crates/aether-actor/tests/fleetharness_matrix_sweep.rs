//! `FleetHarness` cluster-addressing matrix sweep (issue 1977, ADR-0114
//! amendment): load the `matrix_sweep` cluster fixture + a cross-cluster
//! `source_observer`, drive the sweep over the real `WireFrame::Call` wire,
//! read back the structured `MatrixReport`, and assert every cell — delivery
//! AND the source the recipient read (`ctx.sender()`).
//!
//! Cells asserted from the report (in-cluster, in-place dispatch):
//!
//! - parent → child[a]: child[a] received it; its source is the parent id,
//!   anchored by the observer witness below.
//! - child[a] → parent: the parent received it; its source is child[a]'s id
//!   (the in-place "from" half — Task 1).
//! - child[a] → sibling child[b]: child[b] received it; its source is
//!   child[a]'s id.
//! - child[a] → self: child[a] re-received it; its source is its own id.
//!
//! Cells asserted out-of-band through the `source_observer`'s log:
//!
//! - cross-cluster, during the in-place drain: mailed by child[a] during the
//!   `RunMatrix` drain, the observer logs the source it read — child[a]'s id.
//!   The drain re-stamps the host's dispatch identity to the member it
//!   dispatches (validated host-side to the cluster), so a member's own
//!   cross-cluster send carries the member as origin.
//! - the parent witness: the parent queries the observer once before the
//!   fan-out, so the observer logs the host-stamped parent id. No actor reads
//!   its own position (ADR-0230), so this is the independent anchor for the
//!   parent → child[a] source child[a] recorded in place.
//!
//! What this layer proves vs. the unit tests: `FleetHarness` proves to-and-from
//! delivery and the source the recipient reads, end-to-end over the real RPC
//! stack. The in-place *mechanism* (whether a send ran in place vs. via the
//! scheduler) is not externally observable over the wire — that is covered by
//! the Task 1 unit tests in `aether-actor` (`drained_child_reads_*`). The
//! cells here distinguish the directions and the resolved sources, which is
//! what the wire layer can witness.

mod tests {
    use aether_data::Kind;
    use aether_kinds::{LogEntry, LogTailResult};
    use aether_test_fixtures_kinds::{CollectMatrix, MatrixReport, RunMatrix};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// Drive the full cluster-addressing matrix over the wire and assert
    /// every cell: in-cluster delivery + the source each recipient read,
    /// plus the cross-cluster boundary cell observed via the observer's
    /// log.
    #[test]
    fn fleetharness_matrix_sweep_covers_every_addressing_cell() {
        if !dist_component_available("aether_test_fixtures_bundle") {
            return;
        }
        let mut harness = FleetHarness::start();
        let engine = harness.spawn_headless();

        // A separate cross-cluster observer component and the cluster (parent
        // plus two inline children). The observer loads first and under its
        // own namespace: the parent declares it as a dependency, so its route
        // has to be `Live` before the parent's load is accepted.
        let observer = harness.load_full_export(engine, "aether_test_fixtures_bundle", "test.source_observer");
        let parent_addr = harness.load_full_export(engine, "aether_test_fixtures_bundle", "test.matrix.parent").addr;

        // Drive the sweep: the parent fans out every in-cluster direction
        // in place, plus a cross-cluster send to the observer during the
        // drain. The whole cascade settles before this `send` returns.
        let run_replies = harness.send(engine, &parent_addr, &RunMatrix);
        assert!(
            run_replies.is_empty(),
            "RunMatrix is fire-and-settle (no reply), got {} reply events",
            run_replies.len(),
        );

        // Read the cluster's recorded observations.
        let report_replies = harness.send(engine, &parent_addr, &CollectMatrix);
        let report_env = match report_replies.as_slice() {
            [one] => one,
            other => panic!("CollectMatrix should reply exactly one MatrixReport, got {}", other.len()),
        };
        assert_eq!(report_env.kind, MatrixReport::ID, "the CollectMatrix reply should be a MatrixReport");
        let report = MatrixReport::decode_from_bytes(&report_env.payload).expect("the reply decodes as MatrixReport");

        let child_a_id = report.child_a_id;
        let parent_source = report.parent_to_child_source;
        assert_ne!(child_a_id, 0, "the parent recorded child[a]'s id");

        // Cell: parent -> child[a] (in place). child[a] received it and read
        // a source distinct from itself; the observer witness below ties that
        // source to the id the host registered for the parent.
        assert_eq!(report.parent_to_child_arrived, 1, "parent -> child[a] should be delivered");
        assert_ne!(parent_source, 0, "child[a] should read a peer source for parent -> child[a]");
        assert_ne!(
            parent_source, child_a_id,
            "child[a] should read the parent, not itself, as the source of parent -> child[a]",
        );

        // Cell: child[a] -> parent (in place). The parent received it and
        // read child[a]'s id as its source (the Task 1 in-place "from").
        assert_eq!(report.child_to_parent_arrived, 1, "child[a] -> parent should be delivered");
        assert_eq!(
            report.child_to_parent_source, child_a_id,
            "the parent should read child[a]'s id as the source of child[a] -> parent",
        );

        // Cell: child[a] -> sibling child[b] (in place). child[b] received
        // it and read child[a]'s id as its source.
        assert_eq!(report.child_to_sibling_arrived, 1, "child[a] -> sibling child[b] should be delivered");
        assert_eq!(
            report.child_to_sibling_source, child_a_id,
            "child[b] should read child[a]'s id as the source of child[a] -> sibling",
        );

        // Cell: child[a] -> self (in place). child[a] re-received it and
        // read its own id as its source.
        assert_eq!(report.child_to_self_arrived, 1, "child[a] -> self should be delivered");
        assert_eq!(
            report.child_to_self_source, child_a_id,
            "child[a] should read its own id as the source of child[a] -> self",
        );

        let entries = match harness.log_tail(engine, &observer.addr, None, None) {
            LogTailResult::Ok { entries, .. } => entries,
            LogTailResult::Err { error } => panic!("log_tail on observer failed: {error}"),
        };
        let logged: Vec<&LogEntry> = entries.iter().filter(|e| e.message.starts_with("source_mailbox=")).collect();

        // Parent witness: the parent's own cross-cluster query, sent before
        // the fan-out, carries the host-stamped parent id. It must equal the
        // source child[a] read in place for parent -> child[a].
        let parent_expected = format!("source_mailbox={parent_source}");
        assert!(
            logged.iter().any(|e| e.message == parent_expected),
            "the observer should log the parent's host-stamped id {parent_source} — the source \
                 child[a] read for parent -> child[a];\n\
                 expected message: {parent_expected:?}\n\
                 logged source_mailbox entries: {logged:?}",
        );

        // Cross-cluster cell: the observer, mailed by child[a] during the
        // RunMatrix drain, logged the source it read. The drain re-stamps
        // the host's dispatch identity to the member it dispatches before
        // that member's own sends fire, so the host stamps child[a] (not
        // the cluster's inbound parent) as the origin of this send.
        let expected = format!("source_mailbox={child_a_id}");
        assert!(
            logged.iter().any(|e| e.message == expected),
            "the cross-cluster observer should log child[a]'s id {child_a_id} as the source \
                 of a send made during the in-place drain — the drain re-stamps the dispatch \
                 identity to the dispatched member — not the cluster's inbound parent id \
                 {parent_source};\n\
                 expected message: {expected:?}\n\
                 logged source_mailbox entries: {logged:?}",
        );
    }
}
