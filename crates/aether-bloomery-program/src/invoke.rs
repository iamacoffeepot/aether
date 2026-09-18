//! Generic entry that generated bundle code calls for one program.

use alloc::format;
use alloc::vec::Vec;

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, Ref, Refusal};

use crate::declare::Program;
use crate::env::{Env, Pure};
use crate::kinds::Detail;

/// Run `P` against `invoke`. Never returns [`Invoked::Rejected`].
#[must_use]
pub fn invoke<P: Program>(invoke: Invoke) -> Invoked {
    let (seq, _, input, closure) = invoke.into_parts();
    match run::<P>(input, closure) {
        Ok((result, staged)) => Invoked::Completed { seq, result, staged },
        Err(refusal) => Invoked::Refused { seq, refusal },
    }
}

fn run<P: Program>(input: Digest, closure: Vec<ClosureArtifact>) -> Result<(Digest, Vec<EncodedArtifact>), Refusal> {
    let mut env = Env::<Pure>::from_closure(closure);
    let input = env.read(Ref::<P::Input>::from_digest(input))?;
    let result = P::run(input, &mut env)?;
    let result = env.stage_encoded(&result)?;
    refuse_orphans(env.staged(), result.digest())?;
    Ok((result.digest(), env.into_staged()))
}

fn refuse_orphans(staged: &[EncodedArtifact], root: Digest) -> Result<(), Refusal> {
    let reachable = reachable_from(staged, root);
    if let Some(digest) = staged.iter().map(EncodedArtifact::digest).find(|digest| !reachable.contains(digest)) {
        return Err(Refusal::Refused {
            reason: Detail::new(format!("staged blob {digest} is not reachable from the result")),
        });
    }
    Ok(())
}

fn reachable_from(staged: &[EncodedArtifact], root: Digest) -> Vec<Digest> {
    let mut seen = Vec::new();
    let mut stack = alloc::vec![root];
    while let Some(digest) = stack.pop() {
        if seen.contains(&digest) {
            continue;
        }
        seen.push(digest);
        let Some(artifact) = staged.iter().find(|artifact| artifact.digest() == digest) else {
            continue;
        };
        for citation in artifact.citations() {
            let Ok(bytes) = <[u8; 32]>::try_from(citation.bytes()) else {
                continue;
            };
            let child = Digest::from_bytes(bytes);
            if staged.iter().any(|artifact| artifact.digest() == child) {
                stack.push(child);
            }
        }
    }
    seen
}
