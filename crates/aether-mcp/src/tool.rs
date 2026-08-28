//! The generated-handler support types a `#[mcp::tool]` method is authored
//! against: [`Context`] and [`Outcome`].
//!
//! They live in the always-available layer because a tool method's *signature*
//! is vocabulary, not runtime — a provider taking this crate
//! `default-features = false` writes the same method body it would write in a
//! hosted chassis. Only the deferral builder is native, so only that is gated.
//!
//! The pair mirrors `aether_http::typed::Ctx` / `aether_http::typed::Outcome`
//! deliberately: an actor that owns both HTTP routes and tools reads one shape
//! in both places, and the tool surface inherits a design that has already
//! survived the deferred-reply problem.

use std::ops::{Deref, DerefMut};

use aether_data::KindId;

use crate::kinds::ToolError;

/// The context a `#[mcp::tool]` method receives.
///
/// It dereferences to the transport context, so a tool method sends ordinary
/// mail and reaches capabilities exactly as an actor handler does. What it adds
/// is the one fact the method itself cannot know: which tool's hidden request
/// kind is being served. That value is the discriminator a shared reply handler
/// switches on when several tools map the same downstream reply kind, so it has
/// to be carried from dispatch rather than recovered later.
pub struct Context<'a, C> {
    transport: &'a mut C,
    tool_request_kind: KindId,
}

impl<'a, C> Context<'a, C> {
    /// Wrap a transport context with the tool's hidden request kind. Called by
    /// the generated dispatcher, not by hand.
    pub fn new(transport: &'a mut C, tool_request_kind: KindId) -> Self {
        Self { transport, tool_request_kind }
    }

    /// The hidden request kind of the tool currently being served.
    #[must_use]
    pub fn tool_request_kind(&self) -> KindId {
        self.tool_request_kind
    }
}

impl<C> Deref for Context<'_, C> {
    type Target = C;

    fn deref(&self) -> &C {
        self.transport
    }
}

impl<C> DerefMut for Context<'_, C> {
    fn deref_mut(&mut self) -> &mut C {
        self.transport
    }
}

/// What a `#[mcp::tool]` method answers with.
///
/// A tool that decides its own answer returns [`Reply`](Outcome::Reply) —
/// including the failing arm, because a `ToolError` is a *result*, not a
/// protocol fault. A tool that must ask a peer returns
/// [`Deferred`](Outcome::Deferred), which `Context::defer(&request).to::<R>()`
/// produces on the native transport; the reply obligation is already forwarded
/// by then, so the generated dispatcher emits nothing further.
///
/// The `Output` parameter is what keeps a mapping helper honest: the reply
/// mapper bound to a deferred tool must produce that same declared type, and
/// the macro checks it rather than discovering the mismatch at the wire.
pub enum Outcome<Output> {
    /// Answer now with this result.
    Reply(Result<Output, ToolError>),
    /// The request was forwarded to a peer; a reply mapping answers later.
    Deferred,
}

impl<Output> From<Result<Output, ToolError>> for Outcome<Output> {
    fn from(result: Result<Output, ToolError>) -> Self {
        Self::Reply(result)
    }
}

/// The native deferral builder: forward a request to a peer and answer the tool
/// call when that peer's reply lands.
#[cfg(feature = "runtime")]
mod defer {
    use std::marker::PhantomData;

    use aether_actor::{HandlesKind, Manual, Singleton};
    use aether_component::ComponentHostCapability;
    use aether_data::{Kind, Source};
    use aether_substrate::actor::native::{NativeActorMailbox, NativeCtx};

    use crate::kinds::DeferredToolSource;

    use super::{Context, Outcome};

    impl Context<'_, NativeCtx<'_, Manual>> {
        /// Capture `request` and this tool call's reply obligation for deferred
        /// forwarding; [`DeferredToolRequest::to`] names the recipient. Reads
        /// `context.defer(&request).to::<R>()`.
        ///
        /// `to` resolves `R` against the **component host's** carry rather than
        /// the caller's, so one call site addresses both a native root
        /// capability and an embedded component — the same reason
        /// `aether_http::Ctx::defer` does it, and the reason this is not
        /// `context.actor::<R>()`, which supplies the caller's carry and would
        /// refuse an embedded target outright.
        #[must_use = "a deferred request does nothing until `.to::<R>()` forwards it"]
        pub fn defer<'ctx, 'request, K: Kind, Output>(
            &'ctx self,
            request: &'request K,
        ) -> DeferredToolRequest<'ctx, 'request, K, Output> {
            DeferredToolRequest {
                host: self.actor::<ComponentHostCapability>(),
                request,
                source: self.reply_target(),
                tool_request_kind: self.tool_request_kind(),
                output: PhantomData,
            }
        }
    }

    /// A deferred tool call's captured request, produced by
    /// [`Context::defer`]. [`to`](Self::to) resolves the recipient and forwards
    /// the request while the tool call stays open.
    /// `Output` is never named at the `to::<R>()` call site — the recipient is
    /// what the author states, and the declared output arrives later through a
    /// mapping helper. Carrying it as a phantom parameter lets it be inferred
    /// from the tool method's own return type, which is the one place it is in
    /// scope, without a widening conversion the author would have to write.
    pub struct DeferredToolRequest<'ctx, 'request, K, Output> {
        host: NativeActorMailbox<'ctx, ComponentHostCapability>,
        request: &'request K,
        source: Source,
        tool_request_kind: aether_data::KindId,
        output: PhantomData<fn() -> Output>,
    }

    impl<K: Kind, Output> DeferredToolRequest<'_, '_, K, Output> {
        /// Forward this request to recipient `R`, stashing
        /// [`DeferredToolSource`] under the send's correlation so the reply
        /// mapping can recover both the original requester and *which* tool
        /// opened the deferral. The send inherits the handler's causal chain,
        /// so the tool call stays in flight across the round trip.
        /// `R: HandlesKind<K>` compile-checks the request against the recipient.
        #[must_use]
        pub fn to<R>(self) -> Outcome<Output>
        where
            R: Singleton + HandlesKind<K>,
        {
            let recipient = self.host.at::<R>(R::resolve(self.host.mailbox_id().0, ()).0);
            let _ = recipient.send_with_context(
                self.request,
                &DeferredToolSource { source: self.source, tool_request_kind: self.tool_request_kind },
            );
            Outcome::Deferred
        }
    }
}

#[cfg(feature = "runtime")]
pub use defer::DeferredToolRequest;
