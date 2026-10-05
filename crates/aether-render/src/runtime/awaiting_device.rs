//! Requests that need the render device and arrived before the first one
//! existed.
//!
//! Registering a program and creating a texture array both read the device,
//! and on desktop the device is created with the first window, after boot
//! components have already wired. Such a request is not refused: its handler
//! owes the reply with `NativeCtx::defer` (ADR-0243 §1), which holds no
//! settlement, so the sender's chain settles as the handler returns and
//! nothing waits on a window. The mail and its [`Held`] ticket wait here, and
//! installing the first device answers each one, in arrival order, with the
//! answer the device gives it then.
//!
//! Invariant: the list is non-empty only while the device is unbooted. It is
//! never capped and never times out. An entry leaves only when it is
//! answered, or, when the render actor closes first, through the in-flight
//! ledger's `HeldReply::unanswered` (ADR-0243 §1), which settles the entry
//! before the actor's state drops.

use aether_substrate::actor::native::Held;

use crate::{CreateTextureArray, CreateTextureArrayResult, ProgramRegister, ProgramRegisterResult};

/// One request waiting for the first render device: its mail, and the
/// ticket that answers its caller.
pub(super) enum AwaitingDevice {
    ProgramRegister { mail: ProgramRegister, held: Held<ProgramRegisterResult> },
    CreateTextureArray { mail: CreateTextureArray, held: Held<CreateTextureArrayResult> },
}
