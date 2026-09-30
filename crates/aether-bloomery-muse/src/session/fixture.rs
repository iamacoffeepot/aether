//! Test helpers: one sync program run over a value and the closure it cites.

use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, Invoke, Invoked, ProgramName, Refusal};
use aether_bloomery_program::{SyncProgram, invoke};
use aether_data::{Cites, Storage};

use crate::input::{Endpoint, ModelName, OfferedTools, OutputBudget, ReasoningEffort};
use crate::session::state::TurnSettings;

/// `value` as the closure member that stores it.
pub fn stored<K: Storage + Clone + Cites>(value: &K) -> ClosureArtifact {
    let (kind, payload, _) = EncodedArtifact::new(value).expect("a test value encodes").into_parts();
    ClosureArtifact::new(kind, payload)
}

/// Settings posting to a test endpoint and offering `tools`.
pub fn settings(tools: OfferedTools) -> TurnSettings {
    TurnSettings::new(
        Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
        ModelName::new("muse-spark-1.3").expect("model"),
        tools,
        OutputBudget::new(64).expect("budget"),
        ReasoningEffort::Low,
    )
}

/// Run `P` over `input` with `closure` injected beside it, as the driver
/// invokes it, and decode the result it stages.
pub fn run<P: SyncProgram>(input: &P::Input, mut closure: Vec<ClosureArtifact>) -> Result<P::Result, Refusal> {
    let input = stored(input);
    let digest = input.claimed().unverified();
    closure.push(input);
    let name = ProgramName::new(P::NAME).expect("program name");
    match invoke::<P>(Invoke::new(1, name, digest, closure)) {
        Invoked::Completed { result, staged, .. } => {
            let artifact = staged.into_iter().find(|artifact| artifact.digest() == result).expect("result is staged");
            let (kind, payload, _) = artifact.into_parts();
            let payload = ClosureArtifact::new(kind, payload).load(result).expect("staged bytes hash to the result");
            Ok(P::Result::decode_storage(&payload).expect("the result decodes").value)
        }
        Invoked::Refused { refusal, .. } => Err(refusal),
        other => panic!("expected a sync program to complete or refuse, got {other:?}"),
    }
}
