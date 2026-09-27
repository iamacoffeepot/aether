//! `FleetHarness` cluster-addressing matrix sweep (issue 1977, ADR-0114
//! amendment): load the `matrix_sweep` cluster fixture + a cross-cluster
//! `source_observer`, drive the sweep over the real `WireFrame::Call` wire,
//! read back the structured `MatrixReport`, and assert every cell — delivery
//! AND the sender verdict each recipient computed in the guest by comparing
//! `ctx.sender()` with the proof it holds of the expected sender.
//!
//! In-cluster cells (in-place dispatch):
//!
//! - parent → child[a]: child[a] received it; its sender is the proof of its
//!   parent.
//! - child[a] → parent: the parent received it; its sender is the proof of
//!   child[a] (the in-place "from" half — Task 1).
//! - child[a] → sibling child[b]: child[b] received it; its sender is the
//!   proof of child[a].
//! - child[a] → self: child[a] re-received it; its sender is its own proof.
//!
//! Cross-cluster cells, counted by where the `source_observer`'s replies
//! land. The observer replies to the origin the host stamped on each query
//! and sets `had_sender` from its `ctx.sender()`:
//!
//! - the parent witness: the parent queries the observer once before the
//!   fan-out, so exactly one reply lands on the parent.
//! - during the in-place drain: child[a] queries the observer, and the drain
//!   re-stamps the host's dispatch identity to the member it dispatches
//!   (validated host-side to the cluster), so exactly one reply lands on a
//!   child. A mis-stamp as the cluster's inbound parent shows as two replies
//!   on the parent and none on a child.
//!
//! What this layer proves vs. the unit tests: `FleetHarness` proves to-and-from
//! delivery and the sender each recipient reads, end-to-end over the real RPC
//! stack. The in-place *mechanism* (whether a send ran in place vs. via the
//! scheduler) is not externally observable over the wire — that is covered by
//! the Task 1 unit tests in `aether-actor` (`drained_child_reads_*`). The
//! cells here distinguish the directions and the resolved senders, which is
//! what the wire layer can witness.

mod tests {
    use aether_data::Kind;
    use aether_test_fixtures_kinds::{CollectMatrix, MatrixReport, RunMatrix};

    use aether_harness_fleet::{FleetHarness, dist_component_available};

    /// Drive the full cluster-addressing matrix over the wire and assert
    /// every cell: in-cluster delivery + the sender verdict each recipient
    /// computed, plus the cross-cluster cells counted by where the
    /// observer's replies landed.
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
        harness.load_full_export(engine, "aether_test_fixtures_bundle", "test.source_observer");
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

        // Cell: parent -> child[a] (in place).
        assert_eq!(report.parent_to_child_arrived, 1, "parent -> child[a] should be delivered");
        assert_eq!(
            report.parent_to_child_sender_matched, 1,
            "child[a] should read its parent's proof as the sender of parent -> child[a]",
        );

        // Cell: child[a] -> parent (in place, the Task 1 in-place "from").
        assert_eq!(report.child_to_parent_arrived, 1, "child[a] -> parent should be delivered");
        assert_eq!(
            report.child_to_parent_sender_matched, 1,
            "the parent should read child[a]'s proof as the sender of child[a] -> parent",
        );

        // Cell: child[a] -> sibling child[b] (in place).
        assert_eq!(report.child_to_sibling_arrived, 1, "child[a] -> sibling child[b] should be delivered");
        assert_eq!(
            report.child_to_sibling_sender_matched, 1,
            "child[b] should read child[a]'s proof as the sender of child[a] -> sibling",
        );

        // Cell: child[a] -> self (in place).
        assert_eq!(report.child_to_self_arrived, 1, "child[a] -> self should be delivered");
        assert_eq!(
            report.child_to_self_sender_matched, 1,
            "child[a] should read its own proof as the sender of child[a] -> self",
        );

        // Parent witness: the parent's own cross-cluster query, sent before
        // the fan-out, is stamped with the parent, so its reply lands there.
        // Cross-cluster cell: the drain re-stamps the host's dispatch identity
        // to the member it dispatches before that member's own sends fire, so
        // child[a]'s query is stamped with child[a] and its reply lands on a
        // child. A drain that stamped the cluster's inbound parent instead
        // would read 2 and 0.
        assert_eq!(
            (report.observer_reports_to_parent, report.observer_reports_to_child),
            (1, 1),
            "the observer should reply once to the parent (its own query) and once to child[a] \
             (the query child[a] sent during the in-place drain), each with a sender proof; \
             (to_parent, to_child)",
        );
    }
}
