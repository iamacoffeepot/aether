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
    if let Some(digest) = unreachable_staged(staged, root) {
        return Err(Refusal::Refused {
            reason: Detail::new(format!("staged blob {digest} is not reachable from the result")),
        });
    }
    Ok(())
}

/// The first staged digest that `root` cannot reach through staged citations.
///
/// ADR-0224 §3: a completed invocation's staged set is valid whenever every
/// staged blob is reachable from the result, not only when the result is the
/// lone staged artifact. The walk is iterative and shared by every caller
/// that must check this rule — a program's own `refuse_orphans` and, per
/// ADR-0224 §7, the native driver re-checking a bundle's claim rather than
/// trusting it.
#[must_use]
pub fn unreachable_staged(staged: &[EncodedArtifact], root: Digest) -> Option<Digest> {
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
    staged.iter().map(EncodedArtifact::digest).find(|digest| !seen.contains(digest))
}
