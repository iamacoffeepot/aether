//! `POST /proposals` — the signed operator change the coordinator will write
//! (ADR-0205).

use aether_actor::Manual;
use aether_bloomery::{AuthorityDoor, CandidateRef, Event, Fact, IdempotencyKey, OperatorProposal, digest_of};
use aether_data::wire::to_vec;
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::blooms::unstated;
use super::hex::{self, hex_encode};
use super::response::error_response;
use super::state::{ApiCapabilityState, Routed};
use crate::api::dto::ProposeRequest;
use crate::bloomery::{precheck_statement, verified_statement_approval};
use crate::signing::{SigningCapability, Verify, authority_bytes};

impl ApiCapabilityState {
    /// `POST /proposals` — verify the author signature, then admit
    /// [`Fact::ProposeChange`].
    ///
    /// The route is the cryptographic trust gate: it dials `aether.signing` to
    /// verify the statement against the host-custodied allowlist before
    /// admitting, bound to the proposal digest it recomputes from the typed
    /// body (ADR-0182). The reducer independently re-checks that the formed
    /// evidence is an approval of that digest.
    pub(super) fn propose(&self, ctx: &NativeCtx<'_, Manual>, body: &[u8]) -> Routed {
        let request: ProposeRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid proposal body: {error}"))),
        };
        let ProposeRequest { candidate, from_commit, from_worktree, reason, operator, authorization, idempotency_key } =
            request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }
        let candidate = match resolve_proposal_candidate(self, candidate, from_commit, from_worktree) {
            Ok(candidate) => candidate,
            Err(response) => return Routed::Reply(response),
        };

        let proposal = OperatorProposal { candidate, reason, operator };
        let binding = digest_of(&proposal);
        if let Err(error) = precheck_statement(binding, &authorization) {
            return Routed::Reply(error_response(400, &format!("proposal authorization {error:?}")));
        }
        let statement = match to_vec(&authorization) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("authorization encode failed: {error}"))),
        };
        let event = Event {
            idempotency_key: IdempotencyKey(
                idempotency_key
                    .unwrap_or_else(|| format!("aether.bloomery.proposal:{}", hex_encode(binding.as_bytes()))),
            ),
            fact: Fact::ProposeChange { proposal, authorization: verified_statement_approval(binding, &authorization) },
        };
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            &Verify { statement, authority: authority_bytes(AuthorityDoor::Propose, binding), required_tier: None },
        );
        Routed::DeferredVerify { correlation, subject: "proposal authorization", event: Box::new(event) }
    }
}

enum ProposalSource {
    Candidate(CandidateRef),
    FromCommit(String),
    FromWorktree(String),
}

fn proposal_source(
    candidate: Option<CandidateRef>,
    from_commit: Option<String>,
    from_worktree: Option<String>,
) -> Result<ProposalSource, HttpServerResponse> {
    match (candidate, from_commit, from_worktree) {
        (Some(candidate), None, None) => Ok(ProposalSource::Candidate(candidate)),
        (None, Some(commit), None) => Ok(ProposalSource::FromCommit(commit)),
        (None, None, Some(path)) => Ok(ProposalSource::FromWorktree(path)),
        _ => Err(error_response(400, "proposal needs exactly one of `candidate`, `from_commit`, or `from_worktree`")),
    }
}

fn resolve_proposal_candidate(
    state: &ApiCapabilityState,
    candidate: Option<CandidateRef>,
    from_commit: Option<String>,
    from_worktree: Option<String>,
) -> Result<CandidateRef, HttpServerResponse> {
    match proposal_source(candidate, from_commit, from_worktree)? {
        ProposalSource::Candidate(candidate) => Ok(candidate),
        source => derive_proposal_candidate(state, &source),
    }
}

fn derive_proposal_candidate(
    state: &ApiCapabilityState,
    source: &ProposalSource,
) -> Result<CandidateRef, HttpServerResponse> {
    #[cfg(not(feature = "github"))]
    {
        let _ = (state, source);
        Err(error_response(
            422,
            "this chassis cannot derive a candidate from a commit: the GitHub source runtime is not mounted",
        ))
    }

    #[cfg(feature = "github")]
    {
        use std::path::Path;

        use crate::bloomery::{CandidateSource, derive_candidate};

        let Some(correspondence) = state.correspondence.as_ref() else {
            return Err(error_response(
                422,
                "this chassis cannot derive a candidate from a commit: no correspondence store is mounted",
            ));
        };
        let derived = match source {
            ProposalSource::FromCommit(commit) => {
                derive_candidate(correspondence.as_ref(), CandidateSource::Commit(commit), Path::new("."))
            }
            ProposalSource::FromWorktree(path) => {
                derive_candidate(correspondence.as_ref(), CandidateSource::Worktree(Path::new(path)), Path::new("."))
            }
            ProposalSource::Candidate(_) => {
                return Err(error_response(500, "derive_proposal_candidate was handed a pre-built candidate"));
            }
        };
        derived.map(|derived| derived.candidate).map_err(|error| error_response(422, &error.to_string()))
    }
}
