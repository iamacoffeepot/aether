//! The two builders' terminals: which one a caller can reach, and what the
//! eager one carries into a birth.

use std::sync::Arc;

use aether_actor::Instanced;
use aether_data::Kind as _;

use crate::actor::native::NativeActor;
use crate::actor::native::spawn::{HandlerSpawnBuilder, SpawnBuilder, SpawnError, Subname};
use crate::mail::{MailboxId, Source};
use crate::testing::boot_authority;

use super::support::{ActivationConfig, ActivationPoke, ActivationProbe, activation_fixture};

/// an inherent method ahead of a trait one, so these are reachable through
/// method-call syntax on a [`HandlerSpawnBuilder`] exactly while that type
/// declares no inherent `finish` / `finish_with_name` of its own.
trait EagerTerminalProbe {
    fn finish(self) -> &'static str;
    fn finish_with_name(self) -> &'static str;
}

impl<A: Instanced + NativeActor> EagerTerminalProbe for HandlerSpawnBuilder<'_, A> {
    fn finish(self) -> &'static str {
        "probe"
    }

    fn finish_with_name(self) -> &'static str {
        "probe"
    }
}

/// The shape of a boot/embedder eager terminal, over whatever success value
/// it hands back.
type EagerTerminal<'ctx, A, R> = fn(SpawnBuilder<'ctx, A>) -> Result<R, SpawnError>;

/// Tripwire (ADR-0165, iamacoffeepot/aether#4070): a handler builder has no
/// eager terminal, and the boot/embedder builder still has one.
///
/// Both bindings are compile-time assertions — the bodies never run, and
/// the plausible bug is a future edit re-exposing synchronous commit to
/// handler code. Re-adding `HandlerSpawnBuilder::finish` (or
/// `finish_with_name`) makes the inherent method win method resolution
/// above, so the `&'static str` bindings stop type-checking against
/// `Result<MailboxId, SpawnError>` and this file fails to compile.
/// Deleting the boot terminals breaks the paired coercions below, so the
/// asymmetry is pinned from both sides rather than only one.
#[allow(dead_code, reason = "the compile is the assertion; there is no handler binding to construct here")]
fn spawn_terminals_stay_split<'ctx, A: Instanced + NativeActor>(
    staged_only: HandlerSpawnBuilder<'ctx, A>,
    staged_only_named: HandlerSpawnBuilder<'ctx, A>,
) {
    let _: &'static str = staged_only.finish();
    let _: &'static str = staged_only_named.finish_with_name();

    let _: EagerTerminal<'ctx, A, MailboxId> = SpawnBuilder::finish;
    let _: EagerTerminal<'ctx, A, (MailboxId, String)> = SpawnBuilder::finish_with_name;
}

#[test]
fn prepared_bootstrap_mail_shares_the_registered_kind_name() {
    let (spawner, registry, _mailer, pool) = activation_fixture();
    registry
        .register_kind_with_descriptor(
            &boot_authority(),
            aether_data::KindDescriptor {
                name: ActivationPoke::NAME.to_owned(),
                schema: <ActivationPoke as aether_data::Schema>::SCHEMA,
            },
        )
        .unwrap();
    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let builder = SpawnBuilder::<ActivationProbe>::new(
        Arc::clone(&spawner),
        Subname::Named("shared-bootstrap-name"),
        ActivationConfig::new(events_tx),
        (),
        Source::NONE,
    )
    .after_init(ActivationPoke);

    assert_eq!(builder.after_init.len(), 1);
    assert_eq!(builder.after_init[0].kind, ActivationPoke::ID, "bootstrap preparation carries the kind id forward");

    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
