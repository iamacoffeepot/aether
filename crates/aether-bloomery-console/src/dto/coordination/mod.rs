//! Version-tolerant projection of the selected eager head and shared runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

mod json;

use super::{CandidateRef, DigestHex, MemberView, PrecheckView, StageId};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CoordinationView {
    #[serde(default)]
    pub policy: CoordinationPolicyView,
    #[serde(default)]
    pub integration: EagerIntegrationView,
    #[serde(default)]
    pub contexts: BTreeMap<String, ConstructContextView>,
    #[serde(default)]
    pub runs: Vec<SharedRunView>,
    #[serde(default)]
    pub partial_head_repair: Option<PartialHeadRepairView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CoordinationPolicyView {
    #[serde(default)]
    pub eager_integration: bool,
    #[serde(default)]
    pub verification: String,
}

impl CoordinationPolicyView {
    fn uses_selected_head(&self) -> bool {
        self.eager_integration || self.verification == "Contextual"
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartialHeadRepairView {
    #[serde(default)]
    pub head: IntegrationHeadView,
    #[serde(default)]
    pub attempt: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EagerIntegrationView {
    #[serde(default)]
    pub head: IntegrationHeadView,
    #[serde(default)]
    pub known_red: Option<DigestHex>,
    #[serde(default)]
    pub reservation: Option<HeadReservationView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IntegrationHeadView {
    #[serde(default)]
    pub node: DigestHex,
    #[serde(default)]
    pub candidate: CandidateRef,
    #[serde(default)]
    pub coverage: Vec<MemberPinView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberPinView {
    #[serde(default)]
    pub workpiece: String,
    #[serde(default)]
    pub scope_revision: DigestHex,
    #[serde(default)]
    pub candidate: CandidateRef,
}

impl MemberPinView {
    fn matches(&self, member: &MemberView) -> bool {
        self.workpiece == member.workpiece
            && self.scope_revision == member.scope_revision
            && member.cursor.as_ref().and_then(|cursor| cursor.candidate.as_ref()).is_some_and(|candidate| {
                candidate.tree == self.candidate.tree && candidate.checkout == self.candidate.checkout
            })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConstructContextView {
    #[serde(default)]
    pub bloom_base: CandidateRef,
    #[serde(default)]
    pub starting_head: IntegrationHeadView,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeadReservationView {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub node: DigestHex,
    #[serde(default)]
    pub deadline_unix_millis: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SharedRunView {
    #[serde(default)]
    pub plan: SharedRunPlanView,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub physical_run: Option<DigestHex>,
    #[serde(default)]
    pub unfinished: Vec<DigestHex>,
    #[serde(default)]
    pub latencies: Vec<MemberLatencyView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberLatencyView {
    #[serde(default)]
    pub member: MemberPinView,
    #[serde(default)]
    pub latency_millis: u64,
}

impl SharedRunView {
    fn is_live(&self) -> bool {
        matches!(self.phase.as_str(), "Preparing" | "Ready" | "Running")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SharedRunPlanView {
    #[serde(default)]
    pub requests: Vec<MemberRequestView>,
}

#[derive(Debug, Clone, Default)]
pub struct MemberRequestView {
    pub member: MemberPinView,
    pub id: Option<DigestHex>,
}

impl<'de> Deserialize<'de> for MemberRequestView {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let member =
            value.get("member").and_then(|member| serde_json::from_value(member.clone()).ok()).unwrap_or_default();
        // Unknown future request shapes remain readable. A request labels a
        // live row only when its complete canonical identity is in unfinished.
        let id = json::from_value::<aether_bloomery::MemberVerifyRequest>(value)
            .ok()
            .map(|request| DigestHex::from_bytes(*request.digest().as_bytes()));
        Ok(Self { member, id })
    }
}

impl CoordinationView {
    #[must_use]
    pub fn summary(&self, total_members: usize) -> String {
        if !self.policy.uses_selected_head() {
            return match self.policy.verification.as_str() {
                "WarmSerial" => "warm serial verification",
                "Standalone" => "standalone verification",
                _ => "shared verification",
            }
            .to_owned();
        }
        let head = &self.integration.head;
        let state = if self.partial_head_repair.as_ref().is_some_and(|repair| {
            repair.head.node == head.node && repair.head.candidate.checkout == head.candidate.checkout
        }) {
            "repair"
        } else if self.integration.known_red == Some(head.node) {
            "red"
        } else {
            "head"
        };
        format!("{state} {} {}/{}", head.candidate.checkout.prefix(), head.coverage.len(), total_members)
    }

    #[must_use]
    pub fn precheck_status(&self, precheck: Option<&PrecheckView>) -> String {
        let Some(precheck) = precheck else {
            return "off".to_owned();
        };
        if self.policy.uses_selected_head()
            && let Some(node) = precheck.final_join.as_ref().or(precheck.prepared.as_ref()).or(precheck.issued.as_ref())
            && node.tree != self.integration.head.candidate.tree
        {
            return "stale".to_owned();
        }
        match precheck.summary().split_whitespace().next().unwrap_or("pending") {
            "running" | "joined" | "preparing" | "waiting" => "pending".to_owned(),
            status => status.to_owned(),
        }
    }

    /// A removed or replaced candidate cannot retain a folded label merely
    /// because the same workpiece appeared on an older head.
    #[must_use]
    pub fn member_summary(&self, member: &MemberView) -> String {
        if member.withdrawn.is_some() {
            return "withdrawn".to_owned();
        }
        let context = self.contexts.get(&member.workpiece);
        let target = context.map(|context| context.starting_head.candidate.checkout.prefix());
        if member.cursor.as_ref().and_then(|cursor| cursor.stage) == Some(StageId::Reconcile) {
            let state = if member.park.is_some() || member.awaiting_surface.is_some() || member.wedge.is_some() {
                "collided"
            } else {
                "reconciling"
            };
            return target.map_or_else(|| state.to_owned(), |target| format!("{state} {target}"));
        }
        if self.integration.head.coverage.iter().any(|pin| pin.matches(member)) {
            return format!("folded {}", self.integration.head.candidate.checkout.prefix());
        }
        if let Some(run) = self.runs.iter().rev().find(|run| {
            run.is_live()
                && !run.stale
                && run.plan.requests.iter().any(|request| {
                    request.member.matches(member) && request.id.is_some_and(|id| run.unfinished.contains(&id))
                })
        }) {
            if run.phase != "Running" {
                return "shared queued".to_owned();
            }
            return run
                .physical_run
                .map_or_else(|| "shared queued".to_owned(), |run| format!("shared {}", run.prefix()));
        }
        target.map_or_else(|| "pending fold".to_owned(), |target| format!("on {target}"))
    }

    /// Latest latency for this exact member revision and candidate. A shared
    /// run's physical cost remains bloom-level and is never copied here.
    #[must_use]
    pub fn member_latency(&self, member: &MemberView) -> Option<(Option<DigestHex>, u64)> {
        self.runs.iter().rev().find_map(|run| {
            run.latencies
                .iter()
                .find(|latency| latency.member.matches(member))
                .map(|latency| (run.physical_run, latency.latency_millis))
        })
    }

    #[must_use]
    pub fn detail_lines(&self, total_members: usize) -> Vec<String> {
        let mut lines = vec![self.summary(total_members)];
        let coverage = self.integration.head.coverage.iter().map(|pin| pin.workpiece.as_str()).collect::<Vec<_>>();
        if !coverage.is_empty() {
            lines.push(format!("  covered  {}", coverage.join(", ")));
        }
        if let Some(reservation) = &self.integration.reservation {
            lines.push(format!(
                "  reserved {} for {} until {} ms UTC",
                reservation.node.prefix(),
                reservation.owner,
                reservation.deadline_unix_millis
            ));
        }
        if let Some(repair) = &self.partial_head_repair {
            lines.push(format!(
                "  head repair  {} · attempt {}",
                repair.head.candidate.checkout.prefix(),
                repair.attempt.saturating_add(1)
            ));
        }
        let active = self.runs.iter().filter(|run| run.is_live() && !run.stale && !run.unfinished.is_empty()).count();
        if active > 0 {
            lines.push(format!("  shared runs  {active}"));
        }
        let retiring = self.runs.iter().filter(|run| run.is_live() && run.stale).count();
        if retiring > 0 {
            lines.push(format!("  retiring shared runs  {retiring}"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use crate::dto::PrecheckNodeView;
    use aether_bloomery::{
        BloomId, CompositionInput, ConfigRegistry, Digest, ExecutionLimits, MemberPin, MemberVerifyRequest,
        NetworkProfile, StageCatalog, Transformation, VerificationContract,
    };

    use super::*;
    use crate::dto::CompositionCursorView;

    fn request(workpiece: &str) -> MemberVerifyRequest {
        let candidate =
            aether_bloomery::CandidateRef { tree: Digest::from_bytes([1; 32]), checkout: Digest::from_bytes([2; 32]) };
        let member = MemberPin {
            workpiece: aether_bloomery::WorkpieceId(workpiece.to_owned()),
            scope_revision: Digest::from_bytes([4; 32]),
            candidate,
        };
        MemberVerifyRequest {
            bloom: BloomId(Digest::from_bytes([3; 32])),
            member: member.clone(),
            input: CompositionInput { node: candidate.tree, candidate, members: vec![member] },
            attempt: 1,
            context: None,
            contract: VerificationContract {
                gate_set: Digest::default(),
                obligations: Vec::new(),
                diff_base: candidate,
                invocation: Digest::default(),
                environment: Digest::default(),
                host_class: Digest::default(),
            },
            transformation: Transformation {
                command: "verify.check".to_owned(),
                inputs: vec![candidate.tree],
                checkout: candidate.checkout,
                diff_base: None,
                outputs: Vec::new(),
                image: "native".to_owned(),
                limits: ExecutionLimits { wall_clock_secs: 60 },
                network: NetworkProfile::None,
                description: None,
                model: None,
            },
            profile: StageCatalog::binding_of(aether_bloomery::StageId::Verify).profile,
            configs: ConfigRegistry::default(),
        }
    }

    fn request_view(request: &MemberVerifyRequest) -> MemberRequestView {
        serde_json::from_value(serde_json::to_value(request).expect("complete request fixture"))
            .expect("complete request fixture")
    }

    #[test]
    fn request_identity_accepts_rest_hex_without_rewriting_operator_text() {
        let request = request(&"a".repeat(64));
        let expected = DigestHex::from_bytes(*request.digest().as_bytes());
        let mut value = serde_json::to_value(&request).expect("complete request fixture");
        value["member"]["scope_revision"] = request.member.scope_revision.to_hex().into();
        value["member"]["candidate"]["checkout"] = request.member.candidate.checkout.to_hex().into();
        let decoded: MemberRequestView = serde_json::from_value(value.clone()).expect("complete request fixture");
        assert_eq!(decoded.member.workpiece, request.member.workpiece.0);
        assert_eq!(decoded.id, Some(expected));
        value["transformation"]["network"] = "FutureNetworkVariant".into();
        let newer: MemberRequestView = serde_json::from_value(value).expect("complete request fixture");
        assert_eq!(newer.member.workpiece, request.member.workpiece.0);
        assert_eq!(newer.id, None);
    }

    #[test]
    fn only_the_exact_unfinished_request_shows_its_latest_live_run() {
        let first = request_view(&request("first"));
        let second = request_view(&request("second"));
        let member = MemberView {
            workpiece: first.member.workpiece.clone(),
            scope_revision: first.member.scope_revision,
            cursor: Some(CompositionCursorView {
                candidate: Some(first.member.candidate.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = CoordinationView {
            runs: vec![SharedRunView {
                plan: SharedRunPlanView { requests: vec![first.clone(), second.clone()] },
                phase: "Running".to_owned(),
                physical_run: Some(DigestHex::from_bytes([7; 32])),
                unfinished: vec![
                    first.id.expect("complete request fixture"),
                    second.id.expect("complete request fixture"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(state.member_summary(&member), "shared 07070707");
        state.runs[0].unfinished = vec![second.id.expect("complete request fixture")];
        assert_eq!(state.member_summary(&member), "pending fold");
        state.runs.push(SharedRunView {
            plan: SharedRunPlanView { requests: vec![first.clone()] },
            phase: "Running".to_owned(),
            physical_run: Some(DigestHex::from_bytes([8; 32])),
            unfinished: vec![first.id.expect("complete request fixture")],
            ..Default::default()
        });
        assert_eq!(state.member_summary(&member), "shared 08080808");
        state.runs[1].stale = true;
        assert_eq!(state.member_summary(&member), "pending fold");
        assert!(state.detail_lines(2).iter().any(|line| line.contains("retiring shared runs  1")));
        state.runs[1].stale = false;
        // Unissued work is queued until the record becomes terminal. Terminal
        // partial runs can retain unfinished ids without remaining live work.
        state.runs[1].phase = "Ready".to_owned();
        assert_eq!(state.member_summary(&member), "shared queued");
        state.runs[1].phase = "Terminal".to_owned();
        assert_eq!(state.member_summary(&member), "pending fold");
        state.runs[1].phase = "FuturePhase".to_owned();
        assert_eq!(state.member_summary(&member), "pending fold");
    }

    #[test]
    fn journal_run_arrays_decode_with_a_physical_reference() {
        let state: CoordinationView = serde_json::from_value(serde_json::json!({
            "runs": [{
                "physical_run": vec![7; 32],
                "phase": "Running",
                "unfinished": [vec![8; 32]],
                "plan": {"requests": []}
            }]
        }))
        .expect("complete request fixture");
        assert_eq!(state.runs.len(), 1);
        assert_eq!(state.runs[0].physical_run, Some(DigestHex::from_bytes([7; 32])));
        assert!(state.detail_lines(1).iter().any(|line| line.contains("shared runs  1")));
    }

    #[test]
    fn a_replaced_candidate_is_not_shown_folded_with_its_predecessor() {
        let prior = DigestHex::from_bytes([1; 32]);
        let current = DigestHex::from_bytes([2; 32]);
        let state = CoordinationView {
            integration: EagerIntegrationView {
                head: IntegrationHeadView {
                    coverage: vec![MemberPinView {
                        workpiece: "member".to_owned(),
                        candidate: CandidateRef { tree: prior, checkout: prior },
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut member = MemberView {
            workpiece: "member".to_owned(),
            cursor: Some(CompositionCursorView {
                candidate: Some(CandidateRef { tree: current, checkout: current }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(state.member_summary(&member), "pending fold");
        member.cursor.as_mut().expect("complete request fixture").candidate =
            Some(CandidateRef { tree: prior, checkout: prior });
        assert!(state.member_summary(&member).starts_with("folded "));
    }

    #[test]
    fn a_precheck_on_a_superseded_tree_is_shown_stale() {
        let mut state = CoordinationView::default();
        state.policy.eager_integration = true;
        state.integration.head.candidate.tree = DigestHex::from_bytes([2; 32]);
        let precheck = PrecheckView {
            prepared: Some(PrecheckNodeView { tree: DigestHex::from_bytes([1; 32]), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(state.precheck_status(Some(&precheck)), "stale");
    }

    #[test]
    fn warm_serial_precheck_uses_its_own_fold_instead_of_an_unused_eager_head() {
        let state = CoordinationView {
            policy: CoordinationPolicyView { verification: "WarmSerial".to_owned(), eager_integration: false },
            ..Default::default()
        };
        let precheck = PrecheckView {
            prepared: Some(PrecheckNodeView { tree: DigestHex::from_bytes([1; 32]), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(state.precheck_status(Some(&precheck)), "pending");
        assert_eq!(state.summary(2), "warm serial verification");
    }

    #[test]
    fn shared_latency_does_not_migrate_to_another_revision_or_checkout() {
        let candidate = CandidateRef { tree: DigestHex::from_bytes([1; 32]), checkout: DigestHex::from_bytes([2; 32]) };
        let scope_revision = DigestHex::from_bytes([3; 32]);
        let physical_run = DigestHex::from_bytes([7; 32]);
        let state = CoordinationView {
            runs: vec![SharedRunView {
                physical_run: Some(physical_run),
                latencies: vec![MemberLatencyView {
                    member: MemberPinView {
                        workpiece: "member".to_owned(),
                        scope_revision,
                        candidate: candidate.clone(),
                    },
                    latency_millis: 1_250,
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut member = MemberView {
            workpiece: "member".to_owned(),
            scope_revision,
            cursor: Some(CompositionCursorView { candidate: Some(candidate), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(state.member_latency(&member), Some((Some(physical_run), 1_250)));
        member.scope_revision = DigestHex::from_bytes([4; 32]);
        assert_eq!(state.member_latency(&member), None);
        member.scope_revision = scope_revision;
        member
            .cursor
            .as_mut()
            .expect("complete request fixture")
            .candidate
            .as_mut()
            .expect("complete request fixture")
            .checkout = DigestHex::from_bytes([5; 32]);
        assert_eq!(state.member_latency(&member), None);
    }
}
