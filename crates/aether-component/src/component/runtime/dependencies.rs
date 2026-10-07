//! Declared-dependency refusal (ADR-0230, ADR-0241 §4): dependencies are
//! checked where an actor stands up, and only there. An actor whose
//! `#[actor(depends(R))]` entry has no `Live` route is refused before its
//! `init` runs, at each of the four stand-up sites:
//!
//! - a load: the requested actor, whose load answers `Err`;
//! - a module boot: the boot actor, whose waiting loads answer `Err`;
//! - a republish: each live instance, loaded or inline, whose successor type
//!   adds a dependency, all named in the replace's one `Err`;
//! - an inline spawn: the `spawn_inline_child_p32` host fn, which answers
//!   the guest `SpawnError::DependencyNotLive` and stages no alias.
//!
//! Publishing a module stands nothing up, so a load checks no actor the
//! module might spawn later.
//!
//! Every site asks one read, the ctx's `missing_dependency`, whose
//! fold-and-liveness core is the registry's own for both transports. Every
//! declarable dependency is a root singleton (ADR-0241 §5), so the read takes
//! no placement.

use aether_actor::ReplyMode;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::wasm::kind_manifest::Dependency;

/// The refusal error naming the actor and its missing dependency. One
/// constructor for the load, boot, and republish sites, so they cannot
/// silently disagree on the wording.
pub(super) fn dependency_refusal(actor: &str, namespace: &str) -> String {
    format!("{actor} depends on {namespace}, which is not live")
}

/// The refusal error for a republish one of whose live instances' successor
/// type adds a dependency with no `Live` route, naming every such instance
/// by its path, or `None` when every instance may be rebuilt (ADR-0241 §4).
/// Each instance, a loaded member or an inline child one rebuilds, is its
/// path, its type's dependencies before the republish, and after. A
/// dependency the type already declared was live when the instance was
/// created, so only an added one is checked.
pub(super) fn replacement_refusal<'a, A, S, M: ReplyMode>(
    ctx: &NativeCtx<'_, A, S, M>,
    instances: impl IntoIterator<Item = (&'a str, &'a [Dependency], &'a [Dependency])>,
) -> Option<String> {
    let refused: Vec<String> = instances
        .into_iter()
        .filter_map(|(path, before, after)| {
            let added: Vec<Dependency> = after
                .iter()
                .filter(|dependency| before.iter().all(|kept| kept.namespace != dependency.namespace))
                .cloned()
                .collect();
            ctx.missing_dependency(&added).map(|namespace| dependency_refusal(path, namespace))
        })
        .collect();
    (!refused.is_empty()).then(|| refused.join("; "))
}
