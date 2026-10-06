//! Inline-child alias staging (ADR-0114, ADR-0231 §4): the logical routes a
//! guest's inline spawns and despawns publish and retire through the
//! registry owner.

use aether_actor::Single;
use aether_substrate::actor::native::{NativeCtx, RegistryBatch, RegistryBatchResult, TaskDone};
use aether_substrate::mail::registry::{PreparedAliasRetirement, PreparedAliasRoute};

use super::state::{Slot, WasmTrampolineState};

impl WasmTrampolineState {
    /// Publish the logical inline-child routes a guest call staged. The
    /// owner batch is reserved admission; completion is a later no-reply
    /// actor turn so rejection cannot silently lose the originating chain.
    pub fn stage_inline_aliases<A>(ctx: &mut NativeCtx<'_, A, Single>, aliases: Vec<PreparedAliasRoute>) {
        for alias in aliases {
            let context = InlineAliasContext { alias: alias.rendered_name.to_string() };
            let _ = ctx.stage_registry_batch(RegistryBatch::publish_alias(alias), context);
        }
    }

    /// Retire the logical inline-child routes a guest call despawned (#4228),
    /// the teardown mirror of [`Self::stage_inline_aliases`]. Each alias closes
    /// here, from this actor's own turn: it tombstones (ADR-0241 §8), so its
    /// key is never spawned again, and fires its departure notices, so a cap
    /// keying rows on the child's stamped identity (ADR-0114 §4) reclaims them;
    /// the route retirement itself is staged through the owner alongside.
    pub fn stage_inline_alias_retirements<A>(
        ctx: &mut NativeCtx<'_, A, Single>,
        aliases: Vec<PreparedAliasRetirement>,
    ) {
        for alias in aliases {
            ctx.close_alias(&alias);
            let context = InlineAliasContext { alias: alias.rendered_name.to_string() };
            let _ = ctx.stage_registry_batch(RegistryBatch::retire_alias(alias), context);
        }
    }

    /// Finish an alias batch the owner applied or refused. The batch owes no
    /// reply; its context names the alias it staged (ADR-0243 §9).
    ///
    /// A published alias has a route, so each watch an inline child made
    /// before its alias had one registers now (ADR-0079 §8), on every guest
    /// the slot holds: the live one, or a prepared slot's kept guest and its
    /// candidate. A refused batch is then logged.
    pub(super) fn finish_inline_aliases<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, Single>,
        done: TaskDone<RegistryBatchResult>,
    ) {
        match &mut self.slot {
            Slot::Live(component) => component.register_published_watches(),
            Slot::Prepared(prepared) => {
                prepared.old.register_published_watches();
                prepared.candidate.register_published_watches();
            }
            Slot::Released => {}
        }

        let Some(InlineAliasContext { alias }) = ctx.take_context() else {
            return;
        };
        if let Err(error) = done.into_output() {
            tracing::warn!(
                target: "aether_component",
                alias = %alias,
                "inline-child alias registry batch failed after owner staging: {error}",
            );
        }
    }
}

/// The context an inline alias batch carries into its task completion: the
/// alias's rendered name, for the refusal's log line.
#[aether_data::kind(name = "aether.component.trampoline.inline_alias", no_serde)]
pub(super) struct InlineAliasContext {
    alias: String,
}
