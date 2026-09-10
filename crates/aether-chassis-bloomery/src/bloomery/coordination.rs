//! Private Git namespace derivation for eager and grouped verification work.

use aether_bloomery::{BloomId, CandidatePreparationPlan, CompatibilityPreviewPlan, Digest, SharedRunPlan};

fn namespace(domain: &[u8], identity: &Digest) -> BloomId {
    let mut bytes = domain.to_vec();
    bytes.extend_from_slice(identity.as_bytes());
    BloomId(Digest::of_wire_bytes(&bytes))
}

pub fn generation_namespace(generation: Digest) -> BloomId {
    namespace(b"aether.bloomery.integration-generation:", &generation)
}

pub fn preparation_namespace(plan: &CandidatePreparationPlan) -> BloomId {
    namespace(b"aether.bloomery.candidate-preparation:", &plan.digest())
}

pub fn compatibility_namespace(plan: &CompatibilityPreviewPlan) -> BloomId {
    namespace(b"aether.bloomery.compatibility-preview:", &plan.digest())
}

pub fn shared_run_namespace(plan: &SharedRunPlan) -> BloomId {
    namespace(b"aether.bloomery.shared-run:", &plan.digest())
}

pub fn shared_probe_namespace(run: Digest, ordinal: u32, plan: Digest) -> BloomId {
    let mut identity = b"aether.bloomery.shared-probe:".to_vec();
    identity.extend_from_slice(run.as_bytes());
    identity.extend_from_slice(&ordinal.to_be_bytes());
    identity.extend_from_slice(plan.as_bytes());
    BloomId(Digest::of_wire_bytes(&identity))
}
