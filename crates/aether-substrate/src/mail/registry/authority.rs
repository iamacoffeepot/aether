//! The compile-time proof that gates the registry's direct write path.
//!
//! ADR-0165 gave the registry an owner: handlers emit effects, the owner
//! applies them, and every steady-state mutation lands through that one
//! writer. The direct path — `Registry::apply_batches`, reached through
//! the eager mutators — was left additive, so two writers still shared one
//! `RwLock` (iamacoffeepot/aether#4156). Boot genuinely needs the direct
//! path: it wants synchronous apply, a typed `BootError`, and
//! read-your-writes before any scheduler thread exists (#4035's carve-out).
//! So the direct mutators are not routed through the owner — they are made
//! unnameable to anything that cannot produce a [`BootAuthority`].
//!
//! The gate is a type, not a check. #4154 is the precedent: a diagnostic
//! `debug_assert` inside the registry's write guard turned a benign
//! concurrent read into process-wide routing death, because ADR-0063 makes
//! poisoning that lock deliberately fatal. A token costs nothing at
//! runtime and cannot fail at runtime at all.

/// Proof that the caller holds the boot path's authority to write the
/// registry directly, without going through the ADR-0165 owner.
///
/// A zero-sized token with a private field, so only `aether-substrate`
/// itself can mint one (`BootAuthority::new` is crate-private) and only the
/// boot-owned infrastructure that holds one can name
/// [`Registry::try_register_inbox_with_id`], `Registry::remove_closure`,
/// `Registry::install_seize_handle`, or
/// [`Registry::register_kind_with_descriptor`]. A capability, a component,
/// or any other downstream crate reaches those mutators only if boot hands
/// it the token; a handler never receives one, so the eager path is
/// unreachable from steady state by construction rather than by
/// convention.
///
/// `RegistryOwnerLease::attach` takes one **by value**: the proof handed to
/// the owner is spent there and cannot be reused for a second writer.
///
/// [`Registry::try_register_inbox_with_id`]: crate::mail::registry::Registry::try_register_inbox_with_id
/// [`Registry::register_kind_with_descriptor`]: crate::mail::registry::Registry::register_kind_with_descriptor
pub struct BootAuthority(());

impl BootAuthority {
    /// Mint the boot path's proof.
    ///
    /// Crate-private on purpose. Every mint is a boot-owned holder taking
    /// authority for its own lifetime — [`SubstrateBoot`](crate::boot::SubstrateBoot)
    /// for the kind-descriptor seed, the chassis builder for the owner
    /// attach and the cap-claim passes, and [`Spawner`](crate::Spawner) for
    /// the boot/embedder eager spawn. Those sites are the whole audit
    /// surface: `grep 'BootAuthority::new'` enumerates every place the
    /// direct writer is still authorized.
    pub(crate) fn new() -> Self {
        Self(())
    }
}
