//! [`HeadlessClipboardCapability`] **identity** (ADR-0122 identity/runtime
//! split): the fail-fast companion for chassis without a clipboard
//! peripheral. Always-on like the primary ZST in the crate root; the runtime
//! half is the nested `runtime::headless` module, covered by the one
//! `mod runtime;` gate.

use aether_actor::actor;

/// Fail-fast `aether.clipboard` companion for chassis without a clipboard.
#[actor(singleton, root, runtime::headless)]
pub struct HeadlessClipboardCapability;
