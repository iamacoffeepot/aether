//! The `aether.gemini` mailbox cap — the `GeminiCapability` actor and
//! its ADR-0093 hold-until-resolve handlers (ADR-0050 / ADR-0074
//! Phase 5). Per-model validation runs synchronously on the dispatcher
//! thread before any off-thread network dispatch; completions route
//! back as `TaskDone` to the `#[handler(task)]` arms.

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits the `impl HandlesKind<K> for X {}` markers always-on
// against the identity (outside the `feature = "runtime"` gate), so they
// reference these kinds from here.
use super::{LyriaGenerate, NanobananaGenerate};

/// `aether.gemini` mailbox cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable`
/// (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind` markers, and
/// the name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`GeminiCapabilityState`,
/// which holds the `aether_substrate`-typed adapter + task queue) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build
/// never names it nor pulls `aether_substrate` through this cap.
#[actor(singleton)]
pub struct GeminiCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the reply helpers — lives in the
// `runtime` module (declared in `gemini/mod.rs`), gated once by
// `feature = "runtime"`; the `#[runtime] impl` sits beside its state there.
use aether_actor::actor;
