//! Kinds the `aether.component` capability owns for its own internal mail.

/// `aether.component.boot_teardown` — the component host tears down a
/// module's boot trampoline (ADR-0147).
///
/// The host sends it through the boot trampoline's proven reference when the
/// module's last non-boot actor unloads, and the trampoline unloads its guest
/// as a drop does. It carries nothing because the recipient is the target,
/// and there is no reply.
#[aether_data::kind(name = "aether.component.boot_teardown", default, no_serde)]
pub struct BootTeardown {}
