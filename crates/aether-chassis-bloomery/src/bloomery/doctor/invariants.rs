//! Named cross-source state invariants the doctor evaluates against live state.
//!
//! The list is code, not config: [`Invariant::ALL`] is the closed seed set.
//! Each check reads the journal snapshot, fleet repository refs, correspondence,
//! outbox, and on-disk evidence together and returns pass/fail plus the concrete
//! divergent values. A violation that only appears in journald is a failure of
//! this design — the report is what `/view` and the operator channel post.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use aether_bloomery::{
    BackendObjectId, BloomId, BloomStatus, ClaimHolder, ClaimRefKind, ClaimRefState, Digest, Snapshot,
    is_active_unlanded,
};
use serde::Serialize;

/// How long an undelivered source-replica topic may sit before it is a
/// violation rather than a retry still in flight.
pub const REPLICA_AGE_BOUND: Duration = Duration::from_mins(5);

/// How many consecutive doctor passes may observe the same undelivered
/// replica topic before a deterministic retry is reported as a violation.
pub const DETERMINISTIC_RETRY_BOUND: u32 = 20;

/// One named state property the doctor evaluates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Invariant {
    /// Every `refs/bloomery/claims/` ref names a Sealed or Resolved bloom.
    ClaimRefsNameActiveBlooms,
    /// The mainline admission ref is held by an active bloom or absent.
    AdmissionRefHeldByActiveOrAbsent,
    /// No tombstone claim ref survives a coordinator boot.
    NoTombstoneClaimRef,
    /// `/view` mainline corresponds to the actual mainline ref head.
    ViewMainlineCorresponds,
    /// A Landed bloom's resolution tree is an ancestor of the daily head.
    LandedResolutionIsAncestor,
    /// The observed head equals the actual daily ref head.
    ObservedHeadEqualsDailyHead,
    /// The correspondence table is a bijection.
    CorrespondenceIsBijection,
    /// Every digest the journal references resolves through correspondence.
    JournalDigestsResolve,
    /// No undelivered replica topic is older than [`REPLICA_AGE_BOUND`].
    ReplicaTopicAge,
    /// A non-terminal member has a live lane or a pending dispatch.
    NonterminalMemberHasLaneOrDispatch,
    /// A deterministic failure retried past [`DETERMINISTIC_RETRY_BOUND`].
    DeterministicRetryBound,
    /// An evidence directory exists for every open dispatch.
    OpenDispatchHasEvidence,
}

impl Invariant {
    /// The closed seed list, in the order an operator reads a report.
    pub const ALL: &'static [Self] = &[
        Self::ClaimRefsNameActiveBlooms,
        Self::AdmissionRefHeldByActiveOrAbsent,
        Self::NoTombstoneClaimRef,
        Self::ViewMainlineCorresponds,
        Self::LandedResolutionIsAncestor,
        Self::ObservedHeadEqualsDailyHead,
        Self::CorrespondenceIsBijection,
        Self::JournalDigestsResolve,
        Self::ReplicaTopicAge,
        Self::NonterminalMemberHasLaneOrDispatch,
        Self::DeterministicRetryBound,
        Self::OpenDispatchHasEvidence,
    ];

    /// The stable machine name `/view` and tests quote.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaimRefsNameActiveBlooms => "claim_refs_name_active_blooms",
            Self::AdmissionRefHeldByActiveOrAbsent => "admission_ref_held_by_active_or_absent",
            Self::NoTombstoneClaimRef => "no_tombstone_claim_ref",
            Self::ViewMainlineCorresponds => "view_mainline_corresponds",
            Self::LandedResolutionIsAncestor => "landed_resolution_is_ancestor",
            Self::ObservedHeadEqualsDailyHead => "observed_head_equals_daily_head",
            Self::CorrespondenceIsBijection => "correspondence_is_bijection",
            Self::JournalDigestsResolve => "journal_digests_resolve",
            Self::ReplicaTopicAge => "replica_topic_age",
            Self::NonterminalMemberHasLaneOrDispatch => "nonterminal_member_has_lane_or_dispatch",
            Self::DeterministicRetryBound => "deterministic_retry_bound",
            Self::OpenDispatchHasEvidence => "open_dispatch_has_evidence",
        }
    }

    /// One-sentence statement of the property.
    #[must_use]
    pub const fn statement(self) -> &'static str {
        match self {
            Self::ClaimRefsNameActiveBlooms => {
                "every ref under refs/bloomery/claims/ names a bloom currently Sealed or Resolved — never Landed, never unknown"
            }
            Self::AdmissionRefHeldByActiveOrAbsent => "the mainline admission ref is held by an active bloom or absent",
            Self::NoTombstoneClaimRef => "no tombstone claim ref survives a coordinator boot",
            Self::ViewMainlineCorresponds => {
                "the /view mainline digest corresponds to the actual mainline ref head through the correspondence store"
            }
            Self::LandedResolutionIsAncestor => {
                "a Landed bloom's resolution tree is an ancestor of the current daily head"
            }
            Self::ObservedHeadEqualsDailyHead => "the observed head equals the actual daily ref head",
            Self::CorrespondenceIsBijection => "the correspondence table is a bijection",
            Self::JournalDigestsResolve => "every digest the journal references resolves through correspondence",
            Self::ReplicaTopicAge => "no undelivered replica topic is older than a bounded age",
            Self::NonterminalMemberHasLaneOrDispatch => {
                "every member in a non-terminal stage has a live lane process or a pending dispatch"
            }
            Self::DeterministicRetryBound => {
                "a deterministic failure retried beyond a bounded count is reported as a violation rather than continuing at warn"
            }
            Self::OpenDispatchHasEvidence => {
                "an evidence directory exists for every dispatch the journal considers open"
            }
        }
    }

    fn divergences(self, live: &LiveState<'_>) -> Vec<String> {
        match self {
            Self::ClaimRefsNameActiveBlooms => claim_refs_name_active_blooms(live),
            Self::AdmissionRefHeldByActiveOrAbsent => admission_ref_held_by_active_or_absent(live),
            Self::NoTombstoneClaimRef => no_tombstone_claim_ref(live),
            Self::ViewMainlineCorresponds => view_mainline_corresponds(live),
            Self::LandedResolutionIsAncestor => landed_resolution_is_ancestor(live),
            Self::ObservedHeadEqualsDailyHead => observed_head_equals_daily_head(live),
            Self::CorrespondenceIsBijection => correspondence_is_bijection(live),
            Self::JournalDigestsResolve => journal_digests_resolve(live),
            Self::ReplicaTopicAge => replica_topic_age(live),
            Self::NonterminalMemberHasLaneOrDispatch => nonterminal_member_has_lane_or_dispatch(live),
            Self::DeterministicRetryBound => deterministic_retry_bound(live),
            Self::OpenDispatchHasEvidence => open_dispatch_has_evidence(live),
        }
    }
}

/// One invariant's verdict in a doctor pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    /// [`Invariant::name`].
    pub name: &'static str,
    /// [`Invariant::statement`].
    pub statement: &'static str,
    /// Whether the property held on this pass.
    pub passed: bool,
    /// Concrete divergent values when [`passed`](Self::passed) is false.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub divergences: Vec<String>,
}

/// The full doctor report: every seed invariant, in [`Invariant::ALL`] order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// One row per seed invariant.
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    /// Whether every invariant passed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    /// The failing rows, in report order.
    pub fn violations(&self) -> impl Iterator<Item = &CheckResult> {
        self.checks.iter().filter(|check| !check.passed)
    }

    /// The named row, if this pass evaluated it.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|check| check.name == name)
    }

    /// A stable fingerprint of the failing set, for change-driven notify.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.violations()
            .map(|check| {
                check
                    .divergences
                    .first()
                    .map_or_else(|| check.name.to_owned(), |first| format!("{}:{first}", check.name))
            })
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// An undelivered source-replica outbox topic as the doctor observed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaObservation {
    /// The outbox sequence still queued.
    pub sequence: u64,
    /// How long this process has seen the entry undelivered.
    pub age: Duration,
    /// Consecutive doctor passes that still found this sequence queued.
    pub consecutive_failures: u32,
}

/// One dispatch the journal still considers open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenDispatch<'a> {
    /// The dispatch nonce — also the evidence-directory stem.
    pub nonce: &'a str,
    /// The member workpiece the order belongs to.
    pub workpiece: &'a str,
}

/// Whether `to` is `from` or a descendant. `None` when the source cannot answer.
pub type Ancestry<'a> = dyn Fn(&Digest, &Digest) -> Option<bool> + 'a;

/// The live seams one doctor pass reads. Pure: evaluate allocates a report
/// and mutates nothing.
pub struct LiveState<'a> {
    /// The journal snapshot this pass rebuilt.
    pub snapshot: &'a Snapshot,
    /// Live claim refs from the fleet repository.
    pub claims: &'a [ClaimRefState],
    /// The digest correspondence assigns the actual daily/mainline ref, when
    /// the live sha is recorded. `None` when the sha has no correspondence.
    pub actual_head: Option<Digest>,
    /// The live daily/mainline ref sha, for divergence text when unresolved.
    pub actual_head_sha: Option<&'a str>,
    /// Every recorded correspondence pair.
    pub correspondence: &'a [(Digest, BackendObjectId)],
    /// Landed bloom → the head `Fact::Land` recorded for it.
    pub landed_heads: &'a [(BloomId, Digest)],
    /// Whether `to` is `from` or a descendant. `None` when the source cannot
    /// answer ancestry (unconfigured, or a digest has no correspondence).
    pub ancestry: Option<&'a Ancestry<'a>>,
    /// Undelivered source-replica topics this process has been watching.
    pub replica: &'a [ReplicaObservation],
    /// Outstanding journal dispatches.
    pub outstanding: &'a [OpenDispatch<'a>],
    /// Whether any lane process is in flight on this host.
    pub lanes_running: bool,
    /// Nonces whose `{nonce}-evidence` directory exists on disk.
    pub evidence_nonces: &'a [&'a str],
}

/// Evaluate every seed invariant against `live`.
#[must_use]
pub fn evaluate(live: &LiveState<'_>) -> DoctorReport {
    let checks = Invariant::ALL
        .iter()
        .copied()
        .map(|invariant| {
            let divergences = invariant.divergences(live);
            CheckResult {
                name: invariant.name(),
                statement: invariant.statement(),
                passed: divergences.is_empty(),
                divergences,
            }
        })
        .collect();
    DoctorReport { checks }
}

fn claim_refs_name_active_blooms(live: &LiveState<'_>) -> Vec<String> {
    let mut divergences = Vec::new();
    for state in live.claims {
        let ClaimRefKind::Workpiece(workpiece) = &state.ref_kind else {
            continue;
        };
        let name = format!("refs/bloomery/claims/{}", workpiece.0);
        match &state.holder {
            ClaimHolder::Tombstoned => {}
            ClaimHolder::Held(bloom) => match live.snapshot.blooms.get(bloom) {
                Some(record) if is_active_unlanded(record.status) => {}
                Some(record) => {
                    divergences.push(format!(
                        "{name} held by {} bloom {}",
                        status_name(record.status),
                        hex_of(&bloom.0)
                    ));
                }
                None => divergences.push(format!("{name} held by unknown bloom {}", hex_of(&bloom.0))),
            },
        }
    }
    divergences
}

fn admission_ref_held_by_active_or_absent(live: &LiveState<'_>) -> Vec<String> {
    let Some(state) = live.claims.iter().find(|state| matches!(state.ref_kind, ClaimRefKind::MainlineAdmission)) else {
        return Vec::new();
    };
    match &state.holder {
        ClaimHolder::Tombstoned => Vec::new(),
        ClaimHolder::Held(bloom) => match live.snapshot.blooms.get(bloom) {
            Some(record) if is_active_unlanded(record.status) => Vec::new(),
            Some(record) => {
                vec![format!(
                    "refs/bloomery/admission/mainline held by {} bloom {}",
                    status_name(record.status),
                    hex_of(&bloom.0)
                )]
            }
            None => vec![format!("refs/bloomery/admission/mainline held by unknown bloom {}", hex_of(&bloom.0))],
        },
    }
}

fn no_tombstone_claim_ref(live: &LiveState<'_>) -> Vec<String> {
    live.claims
        .iter()
        .filter(|state| matches!(state.holder, ClaimHolder::Tombstoned))
        .map(|state| format!("{} is tombstoned", claim_ref_name(&state.ref_kind)))
        .collect()
}

fn view_mainline_corresponds(live: &LiveState<'_>) -> Vec<String> {
    match (live.actual_head, live.actual_head_sha) {
        (Some(actual), _) if actual == live.snapshot.mainline => Vec::new(),
        (Some(actual), sha) => vec![format!(
            "/view mainline {} does not correspond to actual head {}{}",
            hex_of(&live.snapshot.mainline),
            hex_of(&actual),
            sha.map(|sha| format!(" (sha {sha})")).unwrap_or_default()
        )],
        (None, Some(sha)) => {
            vec![format!(
                "/view mainline {} has no correspondence for actual head sha {sha}",
                hex_of(&live.snapshot.mainline)
            )]
        }
        (None, None) => Vec::new(),
    }
}

fn landed_resolution_is_ancestor(live: &LiveState<'_>) -> Vec<String> {
    let Some(daily) = live.actual_head else {
        return live
            .landed_heads
            .iter()
            .map(|(bloom, head)| {
                format!(
                    "landed bloom {} resolution {} cannot be checked; daily head is unresolved",
                    hex_of(&bloom.0),
                    hex_of(head)
                )
            })
            .collect();
    };
    let Some(ancestry) = live.ancestry else {
        return live
            .landed_heads
            .iter()
            .map(|(bloom, head)| {
                format!(
                    "landed bloom {} resolution {} cannot be checked; ancestry is unavailable",
                    hex_of(&bloom.0),
                    hex_of(head)
                )
            })
            .collect();
    };
    let mut divergences = Vec::new();
    for (bloom, head) in live.landed_heads {
        match ancestry(head, &daily) {
            Some(true) => {}
            Some(false) => divergences.push(format!(
                "landed bloom {} resolution {} is not an ancestor of daily head {}",
                hex_of(&bloom.0),
                hex_of(head),
                hex_of(&daily)
            )),
            None => divergences.push(format!(
                "landed bloom {} resolution {} ancestry against daily head {} did not resolve",
                hex_of(&bloom.0),
                hex_of(head),
                hex_of(&daily)
            )),
        }
    }
    divergences
}

fn observed_head_equals_daily_head(live: &LiveState<'_>) -> Vec<String> {
    match (live.actual_head, live.actual_head_sha) {
        (Some(actual), _) if actual == live.snapshot.observed => Vec::new(),
        (Some(actual), sha) => vec![format!(
            "observed head {} != actual daily head {}{}",
            hex_of(&live.snapshot.observed),
            hex_of(&actual),
            sha.map(|sha| format!(" (sha {sha})")).unwrap_or_default()
        )],
        (None, Some(sha)) => {
            vec![format!("observed head {} != actual daily sha {sha} (unresolved)", hex_of(&live.snapshot.observed))]
        }
        (None, None) => Vec::new(),
    }
}

fn correspondence_is_bijection(live: &LiveState<'_>) -> Vec<String> {
    let mut seen_digest = HashSet::new();
    let mut seen_object = HashSet::new();
    let mut divergences = Vec::new();
    for (digest, object) in live.correspondence {
        if !seen_digest.insert(*digest) {
            divergences.push(format!("digest {} maps to more than one backend object", hex_of(digest)));
        }
        if !seen_object.insert(object.as_bytes()) {
            divergences.push(format!("backend object maps to more than one digest (also {})", hex_of(digest)));
        }
    }
    divergences
}

fn journal_digests_resolve(live: &LiveState<'_>) -> Vec<String> {
    let known: BTreeSet<Digest> = live.correspondence.iter().map(|(digest, _)| *digest).collect();
    journal_source_digests(live.snapshot)
        .into_iter()
        .filter(|digest| {
            // The genesis sentinel is not a source object until boot records
            // it. An empty correspondence table is an unconfigured or pre-seed
            // host, not a missing mapping for every bloom that still names it.
            if *digest == Snapshot::GENESIS_MAINLINE && live.correspondence.is_empty() {
                return false;
            }
            !known.contains(digest)
        })
        .map(|digest| format!("journal digest {} has no correspondence", hex_of(&digest)))
        .collect()
}

fn replica_topic_age(live: &LiveState<'_>) -> Vec<String> {
    live.replica
        .iter()
        .filter(|topic| topic.age > REPLICA_AGE_BOUND)
        .map(|topic| {
            format!(
                "source-replica sequence {} undelivered for {} secs (bound {})",
                topic.sequence,
                topic.age.as_secs(),
                REPLICA_AGE_BOUND.as_secs()
            )
        })
        .collect()
}

fn nonterminal_member_has_lane_or_dispatch(live: &LiveState<'_>) -> Vec<String> {
    let pending: BTreeSet<&str> = live.outstanding.iter().map(|open| open.workpiece).collect();
    let mut divergences = Vec::new();
    for (bloom, record) in &live.snapshot.blooms {
        if record.status != BloomStatus::Sealed || record.operator_hold.is_some() || record.review_park.is_some() {
            continue;
        }
        for workpiece in record.progress.keys() {
            if record.wedged.contains_key(workpiece)
                || record.claims.contains_key(workpiece)
                || record.host_faults.contains_key(workpiece)
            {
                continue;
            }
            if pending.contains(workpiece.0.as_str()) || live.lanes_running {
                continue;
            }
            divergences.push(format!(
                "member {} on bloom {} is at a non-terminal stage with no live lane and no pending dispatch",
                workpiece.0,
                hex_of(&bloom.0)
            ));
        }
    }
    divergences
}

fn deterministic_retry_bound(live: &LiveState<'_>) -> Vec<String> {
    live.replica
        .iter()
        .filter(|topic| topic.consecutive_failures > DETERMINISTIC_RETRY_BOUND)
        .map(|topic| {
            format!(
                "source-replica sequence {} retried {} times (bound {})",
                topic.sequence, topic.consecutive_failures, DETERMINISTIC_RETRY_BOUND
            )
        })
        .collect()
}

fn open_dispatch_has_evidence(live: &LiveState<'_>) -> Vec<String> {
    let present: BTreeSet<&str> = live.evidence_nonces.iter().copied().collect();
    live.outstanding
        .iter()
        .filter(|open| !present.contains(open.nonce))
        .map(|open| format!("open dispatch {} (member {}) has no evidence directory", open.nonce, open.workpiece))
        .collect()
}

fn journal_source_digests(snapshot: &Snapshot) -> BTreeSet<Digest> {
    let mut digests = BTreeSet::new();
    digests.insert(snapshot.mainline);
    digests.insert(snapshot.observed);
    for record in snapshot.blooms.values() {
        digests.insert(record.spec.base());
        if let Some(head) = record.resolved_head {
            digests.insert(head);
        }
        if let Some(fold) = &record.integration {
            digests.insert(fold.tree);
            digests.insert(fold.head);
        }
        for claim in record.claims.values() {
            digests.insert(claim.candidate);
        }
        for progress in record.progress.values() {
            if let Some(candidate) = progress.candidate {
                digests.insert(candidate.tree);
            }
        }
    }
    for by_bloom in snapshot.member_checkpoints.values() {
        for checkpoint in by_bloom.values() {
            digests.insert(checkpoint.tree);
        }
    }
    digests
}

fn claim_ref_name(kind: &ClaimRefKind) -> String {
    match kind {
        ClaimRefKind::Workpiece(workpiece) => format!("refs/bloomery/claims/{}", workpiece.0),
        ClaimRefKind::MainlineAdmission => String::from("refs/bloomery/admission/mainline"),
    }
}

fn status_name(status: BloomStatus) -> &'static str {
    match status {
        BloomStatus::Sealed => "Sealed",
        BloomStatus::Resolved => "Resolved",
        BloomStatus::Landed => "Landed",
        BloomStatus::Superseded => "Superseded",
    }
}

fn hex_of(digest: &Digest) -> String {
    digest.to_hex()
}

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::{digest, draft, membership, splice_bloom};
    use aether_bloomery::{BloomStatus, ClaimHolder, ClaimRefKind, ClaimRefState, Snapshot, WorkpieceId};

    use super::{DETERMINISTIC_RETRY_BOUND, Invariant, LiveState, OpenDispatch, ReplicaObservation, evaluate};

    fn live<'a>(snapshot: &'a Snapshot, claims: &'a [ClaimRefState]) -> LiveState<'a> {
        LiveState {
            snapshot,
            claims,
            actual_head: None,
            actual_head_sha: None,
            correspondence: &[],
            landed_heads: &[],
            ancestry: None,
            replica: &[],
            outstanding: &[],
            lanes_running: false,
            evidence_nonces: &[],
        }
    }

    #[test]
    fn a_claim_ref_naming_a_landed_bloom_is_named_with_the_ref_and_the_bloom() {
        // Tripwire: #5175 stranded every locally-landed bloom's claim refs and
        // the next seal only then refused with ActiveBloomExists naming a
        // Landed bloom. The doctor has to name the ref and the Landed holder
        // — today it reports nothing.
        let spec = draft(0, vec![membership("issue-5175", 1)]).seal();
        let bloom = spec.id();
        let mut snapshot = Snapshot::default();
        splice_bloom(&mut snapshot, &spec, BloomStatus::Landed);
        let claims = [ClaimRefState {
            ref_kind: ClaimRefKind::Workpiece(WorkpieceId("issue-5175".into())),
            holder: ClaimHolder::Held(bloom),
        }];

        let report = evaluate(&live(&snapshot, &claims));
        let check =
            report.named(Invariant::ClaimRefsNameActiveBlooms.name()).expect("the seed list includes claim refs");
        assert!(!check.passed, "a Landed holder is a violation: {check:?}");
        let named = check.divergences.join(" ");
        assert!(named.contains("refs/bloomery/claims/issue-5175"), "the claim ref is named: {named}");
        assert!(named.contains("Landed"), "the holder status is named: {named}");
        assert!(named.contains(&hex_of(&bloom.0)), "the Landed bloom id is named: {named}");
    }

    #[test]
    fn an_observed_head_mismatch_is_named_with_both_digests() {
        // Tripwire: the observed head drifted from the daily ref and nothing
        // was loud until a later seal or land tripped over the split.
        let snapshot = Snapshot { observed: digest(1), ..Snapshot::default() };
        let actual = digest(2);
        let mut state = live(&snapshot, &[]);
        state.actual_head = Some(actual);

        let report = evaluate(&state);
        let check =
            report.named(Invariant::ObservedHeadEqualsDailyHead.name()).expect("the seed list includes observed head");
        assert!(!check.passed, "a mismatch is a violation: {check:?}");
        let named = check.divergences.join(" ");
        assert!(named.contains(&hex_of(&digest(1))), "the observed digest is named: {named}");
        assert!(named.contains(&hex_of(&actual)), "the actual daily head is named: {named}");
    }

    #[test]
    fn a_clean_fixture_passes_every_seed_invariant() {
        // The other half of the acceptance: an empty journal with no live
        // refs, no replica backlog, and no open dispatch must not invent a
        // violation. The twelve names are the contract the report walks.
        let snapshot = Snapshot::default();
        let report = evaluate(&live(&snapshot, &[]));
        assert_eq!(report.checks.len(), Invariant::ALL.len(), "every seed invariant is reported");
        assert!(report.is_clean(), "an empty fixture must pass: {:?}", report.checks);
    }

    #[test]
    fn a_replica_retry_past_the_bound_is_a_named_violation() {
        // A deterministic replica refusal that keeps retrying at warn is the
        // quiet-failure shape #5176 exists to make loud.
        let snapshot = Snapshot::default();
        let replica = [ReplicaObservation {
            sequence: 7,
            age: super::REPLICA_AGE_BOUND,
            consecutive_failures: DETERMINISTIC_RETRY_BOUND + 1,
        }];
        let mut state = live(&snapshot, &[]);
        state.replica = &replica;

        let report = evaluate(&state);
        let check =
            report.named(Invariant::DeterministicRetryBound.name()).expect("the seed list includes retry bound");
        assert!(!check.passed, "a retry past the bound is a violation: {check:?}");
        assert!(check.divergences.join(" ").contains('7'), "the sequence is named: {:?}", check.divergences);
    }

    #[test]
    fn a_missing_evidence_dir_for_an_open_dispatch_is_named() {
        let snapshot = Snapshot::default();
        let outstanding = [OpenDispatch { nonce: "nonce-1", workpiece: "issue-1" }];
        let mut state = live(&snapshot, &[]);
        state.outstanding = &outstanding;

        let report = evaluate(&state);
        let check =
            report.named(Invariant::OpenDispatchHasEvidence.name()).expect("the seed list includes evidence dirs");
        assert!(!check.passed, "a missing evidence dir is a violation: {check:?}");
        let named = check.divergences.join(" ");
        assert!(named.contains("nonce-1"), "the nonce is named: {named}");
        assert!(named.contains("issue-1"), "the member is named: {named}");
    }

    fn hex_of(digest: &aether_bloomery::Digest) -> String {
        super::hex_of(digest)
    }
}
