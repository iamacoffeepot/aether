-- aether store schema snapshot v26
index|active_membership_by_bloom|CREATE INDEX active_membership_by_bloom ON active_membership (bloom)
index|candidate_hash_by_bloom|CREATE INDEX candidate_hash_by_bloom ON candidate_hash (bloom)
index|construct_session_by_slug|CREATE INDEX construct_session_by_slug ON construct_session (slug)
index|outbox_by_topic_delivery|CREATE INDEX outbox_by_topic_delivery ON outbox (topic, delivered, sequence)
index|proof_facts_identity|CREATE UNIQUE INDEX proof_facts_identity ON proof_facts (closure_key, test_id, result, host_class, producing_dispatch)
index|scope_runs_by_commission|CREATE INDEX scope_runs_by_commission ON scope_runs (commission, ordinal)
index|scope_runs_by_nonce|CREATE INDEX scope_runs_by_nonce ON scope_runs (nonce)
index|scope_runs_unique_transition|CREATE UNIQUE INDEX scope_runs_unique_transition ON scope_runs (commission, ordinal, kind)
index|shared_member_verification_queue_order|CREATE INDEX shared_member_verification_queue_order ON shared_member_verification_queue(sequence)
index|shared_run_members_by_run|CREATE INDEX shared_run_members_by_run ON shared_run_members(run, ordinal)
index|shared_run_steps_by_run|CREATE INDEX shared_run_steps_by_run ON shared_run_steps(run, ordinal)
table|active_membership|CREATE TABLE active_membership ( workpiece TEXT PRIMARY KEY, bloom BLOB NOT NULL )
table|adr_transitions|CREATE TABLE adr_transitions ( digest BLOB PRIMARY KEY, adr BLOB NOT NULL REFERENCES adrs(digest), status TEXT NOT NULL CHECK (status IN ('proposed', 'provisional', 'accepted', 'superseded')), canonical BLOB NOT NULL, statement BLOB, signature BLOB, successor BLOB REFERENCES adrs(digest), CHECK ( (status = 'accepted' AND signature IS NOT NULL AND length(signature) > 0 AND statement IS NOT NULL) OR (status IN ('proposed', 'provisional', 'superseded') AND signature IS NULL AND statement IS NULL) ), CHECK ( (status = 'superseded' AND successor IS NOT NULL) OR (status != 'superseded' AND successor IS NULL) ) )
table|adrs|CREATE TABLE adrs ( digest BLOB PRIMARY KEY, number INTEGER NOT NULL UNIQUE CHECK (number > 0), title TEXT NOT NULL, canonical BLOB NOT NULL )
table|authorized_instructions|CREATE TABLE authorized_instructions ( digest BLOB PRIMARY KEY )
table|candidate_commit_message|CREATE TABLE candidate_commit_message ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, message TEXT NOT NULL, PRIMARY KEY (bloom, workpiece) )
table|candidate_hash|CREATE TABLE candidate_hash ( sequence INTEGER PRIMARY KEY AUTOINCREMENT, bloom BLOB NOT NULL, workpiece TEXT NOT NULL, ref_name TEXT NOT NULL, commit_hex TEXT NOT NULL, occasion TEXT NOT NULL, published INTEGER NOT NULL, recorded_unix_millis INTEGER NOT NULL )
table|capture_diff|CREATE TABLE capture_diff ( nonce TEXT PRIMARY KEY, diff TEXT NOT NULL )
table|commission_approvals|CREATE TABLE commission_approvals ( digest BLOB PRIMARY KEY, commission TEXT NOT NULL REFERENCES commissions(id), scope_digest BLOB NOT NULL REFERENCES scope_revisions(digest), tier TEXT NOT NULL CHECK (tier IN ('signed', 'auto')), statement BLOB NOT NULL, signature BLOB, CHECK ( (tier = 'signed' AND signature IS NOT NULL AND length(signature) > 0) OR (tier = 'auto' AND signature IS NULL) ) )
table|commission_projections|CREATE TABLE commission_projections ( commission TEXT PRIMARY KEY REFERENCES commissions(id), issue_number INTEGER NOT NULL CHECK (issue_number > 0) )
table|commission_statements|CREATE TABLE commission_statements ( digest BLOB PRIMARY KEY, commission TEXT NOT NULL REFERENCES commissions(id), role TEXT NOT NULL CHECK (role IN ('intent', 'cancel')), canonical BLOB NOT NULL )
table|commissions|CREATE TABLE commissions ( id TEXT PRIMARY KEY, intent BLOB NOT NULL, current_revision BLOB, current_ordinal INTEGER, status TEXT NOT NULL CHECK (status IN ('open', 'cancelled', 'landed')) )
table|config|CREATE TABLE config ( digest BLOB PRIMARY KEY, kind TEXT NOT NULL, bytes BLOB NOT NULL, schema_digest BLOB )
table|construct_session|CREATE TABLE construct_session ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, harness_session_id TEXT NOT NULL, context_tokens INTEGER NOT NULL, deposited_unix INTEGER, slug TEXT, PRIMARY KEY (bloom, workpiece) )
table|construction_admissions|CREATE TABLE construction_admissions ( dispatch BLOB PRIMARY KEY, nonce TEXT NOT NULL UNIQUE, queued_unix_millis INTEGER NOT NULL, deadline_unix_millis INTEGER NOT NULL, submitted INTEGER NOT NULL DEFAULT 0, journaled INTEGER NOT NULL DEFAULT 0, retired INTEGER NOT NULL DEFAULT 0 )
table|contextual_dispatches|CREATE TABLE contextual_dispatches ( nonce TEXT PRIMARY KEY, dispatch BLOB NOT NULL )
table|dispatch_description|CREATE TABLE dispatch_description ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, description TEXT NOT NULL, PRIMARY KEY (bloom, workpiece) )
table|dispatch_owners|CREATE TABLE dispatch_owners ( nonce TEXT PRIMARY KEY, bloom BLOB NOT NULL )
table|flake_registry|CREATE TABLE flake_registry ( test_id TEXT NOT NULL, candidate BLOB NOT NULL, PRIMARY KEY (test_id, candidate) )
table|fold_conflict|CREATE TABLE fold_conflict ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, overlay TEXT NOT NULL, PRIMARY KEY (bloom, workpiece) )
table|journal|CREATE TABLE journal ( sequence INTEGER PRIMARY KEY AUTOINCREMENT, idempotency_key TEXT NOT NULL UNIQUE, event BLOB NOT NULL, decisions BLOB, decider TEXT, decisions_schema TEXT, recorded_unix_millis INTEGER, event_schema BLOB, decisions_schema_digest BLOB )
table|member_dependency|CREATE TABLE member_dependency ( bloom BLOB NOT NULL, member TEXT NOT NULL, depends_on TEXT NOT NULL, PRIMARY KEY (bloom, member, depends_on) )
table|metric_bloom|CREATE TABLE metric_bloom ( bloom BLOB PRIMARY KEY, seal_sequence INTEGER NOT NULL, payload BLOB NOT NULL )
table|metric_cursor|CREATE TABLE metric_cursor ( id INTEGER PRIMARY KEY CHECK (id = 0), through_sequence INTEGER NOT NULL )
table|metric_day|CREATE TABLE metric_day ( label TEXT PRIMARY KEY, payload BLOB NOT NULL )
table|metric_dispatch|CREATE TABLE metric_dispatch ( id TEXT PRIMARY KEY, nonce TEXT, sequence INTEGER NOT NULL, bloom BLOB NOT NULL, payload BLOB NOT NULL, session_reuse_arm TEXT, session_reuse_saved_micro_usd INTEGER, peak_resident_bytes INTEGER, calls_json TEXT )
table|notification_sent|CREATE TABLE notification_sent ( notification_key TEXT PRIMARY KEY, posted_unix_millis INTEGER NOT NULL )
table|outbox_results|CREATE TABLE outbox_results ( sequence INTEGER NOT NULL, ordinal INTEGER NOT NULL, event BLOB NOT NULL, event_schema BLOB NOT NULL, PRIMARY KEY (sequence, ordinal) )
table|outbox|CREATE TABLE outbox ( sequence INTEGER PRIMARY KEY AUTOINCREMENT, topic TEXT NOT NULL, payload BLOB NOT NULL, delivered INTEGER NOT NULL DEFAULT 0, payload_schema TEXT )
table|outstanding_orders|CREATE TABLE outstanding_orders ( nonce TEXT PRIMARY KEY, bloom BLOB NOT NULL, workpiece TEXT NOT NULL, scope_revision BLOB NOT NULL, candidate BLOB NOT NULL, displayed_digest BLOB NOT NULL, stage BLOB NOT NULL, transformation BLOB NOT NULL, configs BLOB NOT NULL, profile BLOB NOT NULL, deadline_unix_millis INTEGER NOT NULL, lifecycle TEXT NOT NULL DEFAULT 'submitted', prompt_manifest BLOB )
table|parked_question|CREATE TABLE parked_question ( bloom BLOB NOT NULL, question BLOB NOT NULL, nonce TEXT NOT NULL, workpiece TEXT NOT NULL, scope_revision BLOB NOT NULL, candidate BLOB NOT NULL, displayed_digest BLOB NOT NULL, stage BLOB NOT NULL, transformation BLOB NOT NULL, configs BLOB NOT NULL, profile BLOB NOT NULL, deadline_unix_millis INTEGER NOT NULL, lifecycle TEXT NOT NULL DEFAULT 'submitted', prompt_manifest BLOB, PRIMARY KEY (bloom, question) )
table|partial_head_repairs|CREATE TABLE partial_head_repairs ( sequence INTEGER PRIMARY KEY, nonce TEXT NOT NULL UNIQUE, dispatch BLOB NOT NULL, completion BLOB, accounting BLOB, result BLOB, settled INTEGER NOT NULL DEFAULT 0 )
table|proof_facts|CREATE TABLE proof_facts ( sequence INTEGER PRIMARY KEY AUTOINCREMENT, closure_key BLOB NOT NULL, test_id TEXT NOT NULL, result TEXT NOT NULL, host_class TEXT NOT NULL, producing_dispatch TEXT NOT NULL, producing_bloom BLOB NOT NULL )
table|review_findings|CREATE TABLE review_findings ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, findings TEXT NOT NULL, PRIMARY KEY (bloom, workpiece) )
table|scope_revisions|CREATE TABLE scope_revisions ( digest BLOB PRIMARY KEY, commission TEXT NOT NULL REFERENCES commissions(id), predecessor BLOB REFERENCES scope_revisions(digest), ordinal INTEGER NOT NULL CHECK (ordinal >= 1), canonical BLOB NOT NULL, UNIQUE (commission, ordinal) )
table|scope_runs|CREATE TABLE scope_runs ( sequence INTEGER PRIMARY KEY AUTOINCREMENT, commission TEXT NOT NULL REFERENCES commissions(id), ordinal INTEGER NOT NULL CHECK (ordinal >= 1), kind TEXT NOT NULL CHECK (kind IN ('enqueued', 'dispatched', 'verdict', 'frozen')), nonce TEXT, intent BLOB, base BLOB, subject BLOB, verdict TEXT, evidence BLOB, revision BLOB, instructions BLOB, model_override BLOB, UNIQUE (commission, ordinal, kind) )
table|scope_verify_reports|CREATE TABLE scope_verify_reports ( revision BLOB PRIMARY KEY, commission TEXT NOT NULL REFERENCES commissions(id), refused INTEGER NOT NULL CHECK (refused IN (0, 1)), canonical BLOB NOT NULL )
table|shared_member_verification_queue|CREATE TABLE shared_member_verification_queue ( request BLOB PRIMARY KEY, sequence INTEGER NOT NULL UNIQUE, payload BLOB NOT NULL, queued_unix_millis INTEGER NOT NULL, deadline_unix_millis INTEGER NOT NULL, scheduled INTEGER NOT NULL DEFAULT 0, proposal BLOB )
table|shared_run_cancellations|CREATE TABLE shared_run_cancellations ( plan BLOB PRIMARY KEY )
table|shared_run_members|CREATE TABLE shared_run_members ( run BLOB NOT NULL, request BLOB NOT NULL, ordinal INTEGER NOT NULL, queued_unix_millis INTEGER NOT NULL, deadline_unix_millis INTEGER NOT NULL, cancelled INTEGER NOT NULL DEFAULT 0, outcome BLOB, latency_millis INTEGER, PRIMARY KEY (run, request), FOREIGN KEY (run) REFERENCES shared_runs(run) )
table|shared_run_proof_reuse|CREATE TABLE shared_run_proof_reuse ( run BLOB PRIMARY KEY, reuse BLOB NOT NULL, FOREIGN KEY (run) REFERENCES shared_runs(run) )
table|shared_run_steps|CREATE TABLE shared_run_steps ( run BLOB NOT NULL, ordinal INTEGER NOT NULL, nonce TEXT NOT NULL UNIQUE, request BLOB, descriptor BLOB NOT NULL, prepared BLOB, receipt BLOB, duration_millis INTEGER, release_physical_run INTEGER NOT NULL, PRIMARY KEY (run, ordinal), FOREIGN KEY (run) REFERENCES shared_runs(run) )
table|shared_runs|CREATE TABLE shared_runs ( run BLOB PRIMARY KEY, nonce TEXT NOT NULL UNIQUE, dispatch BLOB NOT NULL, lifecycle TEXT NOT NULL, next_ordinal INTEGER NOT NULL, deadline_unix_millis INTEGER NOT NULL, charged INTEGER NOT NULL DEFAULT 0, physical_cost BLOB )
table|study_index|CREATE TABLE study_index ( bloom BLOB NOT NULL, attempt_digest BLOB NOT NULL, study_artifact TEXT NOT NULL, PRIMARY KEY (bloom, attempt_digest) )
table|suppression_request|CREATE TABLE suppression_request ( bloom BLOB NOT NULL, workpiece TEXT NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL, lint TEXT NOT NULL, reason TEXT NOT NULL, PRIMARY KEY (bloom, workpiece, path, line, lint) )
trigger|adr_transitions_no_delete|CREATE TRIGGER adr_transitions_no_delete BEFORE DELETE ON adr_transitions BEGIN SELECT RAISE(ABORT, 'adr_transitions rows are immutable'); END
trigger|adr_transitions_no_update|CREATE TRIGGER adr_transitions_no_update BEFORE UPDATE ON adr_transitions BEGIN SELECT RAISE(ABORT, 'adr_transitions rows are immutable'); END
trigger|adrs_no_delete|CREATE TRIGGER adrs_no_delete BEFORE DELETE ON adrs BEGIN SELECT RAISE(ABORT, 'adrs rows are immutable'); END
trigger|adrs_no_update|CREATE TRIGGER adrs_no_update BEFORE UPDATE ON adrs BEGIN SELECT RAISE(ABORT, 'adrs rows are immutable'); END
trigger|commission_approvals_no_delete|CREATE TRIGGER commission_approvals_no_delete BEFORE DELETE ON commission_approvals BEGIN SELECT RAISE(ABORT, 'commission_approvals rows are immutable'); END
trigger|commission_approvals_no_update|CREATE TRIGGER commission_approvals_no_update BEFORE UPDATE ON commission_approvals BEGIN SELECT RAISE(ABORT, 'commission_approvals rows are immutable'); END
trigger|commission_statements_no_delete|CREATE TRIGGER commission_statements_no_delete BEFORE DELETE ON commission_statements BEGIN SELECT RAISE(ABORT, 'commission_statements rows are immutable'); END
trigger|commission_statements_no_update|CREATE TRIGGER commission_statements_no_update BEFORE UPDATE ON commission_statements BEGIN SELECT RAISE(ABORT, 'commission_statements rows are immutable'); END
trigger|scope_revisions_no_delete|CREATE TRIGGER scope_revisions_no_delete BEFORE DELETE ON scope_revisions BEGIN SELECT RAISE(ABORT, 'scope_revisions rows are immutable'); END
trigger|scope_revisions_no_update|CREATE TRIGGER scope_revisions_no_update BEFORE UPDATE ON scope_revisions BEGIN SELECT RAISE(ABORT, 'scope_revisions rows are immutable'); END
trigger|scope_runs_no_delete|CREATE TRIGGER scope_runs_no_delete BEFORE DELETE ON scope_runs BEGIN SELECT RAISE(ABORT, 'scope_runs rows are immutable'); END
trigger|scope_runs_no_update|CREATE TRIGGER scope_runs_no_update BEFORE UPDATE ON scope_runs BEGIN SELECT RAISE(ABORT, 'scope_runs rows are immutable'); END
trigger|scope_verify_reports_no_delete|CREATE TRIGGER scope_verify_reports_no_delete BEFORE DELETE ON scope_verify_reports BEGIN SELECT RAISE(ABORT, 'scope_verify_reports rows are immutable'); END
trigger|scope_verify_reports_no_update|CREATE TRIGGER scope_verify_reports_no_update BEFORE UPDATE ON scope_verify_reports BEGIN SELECT RAISE(ABORT, 'scope_verify_reports rows are immutable'); END
