use super::{Kind, ListKindsResult};
use std::sync::Arc;

// Imports for the `#[cfg(test)]` `RouteInventorySink` loopback fixture
// (issue 2672). Brought into scope (rather than named by absolute path
// inline) to satisfy the `clippy::absolute_paths` restriction.
#[cfg(test)]
use aether_actor::actor;
#[cfg(test)]
use aether_capabilities::engine::kinds::{CallSettled, RouteEnvelope};
#[cfg(test)]
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
#[cfg(test)]
use aether_substrate::chassis::error::BootError;
#[cfg(test)]
use aether_substrate::mail::mailer::Mailer;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// The canned live vocabulary a [`RouteInventorySink`] replies with, plus
/// a counter of how many refresh RPCs it has fielded (issue 2672). Shared
/// by value into the fixture so a test both controls the widened schema
/// the refresh observes and asserts the refresh fired exactly once.
#[cfg(test)]
#[derive(Clone)]
pub(super) struct RouteLoopbackConfig {
    pub(super) reply: ListKindsResult,
    pub(super) calls: Arc<AtomicUsize>,
}

/// `#[cfg(test)]` loopback engines-cap double (issue 2672). Registers at
/// the `aether.engine` mailbox — the id the `RpcServerCapability` routes
/// every `engine = Some` `Call` to via a `RouteEnvelope` — and answers the
/// harness's `aether.inventory.kinds` refresh RPC locally with a canned
/// [`ListKindsResult`], so the
/// field-mismatch refresh-and-retry path in [`Mcp::resolve_and_encode`] is
/// exercised end-to-end without forking a real substrate + proxy.
///
/// Lives at file root (not nested in `mod tests`) so the `#[actor]`
/// macro's marker emission stays addressable, mirroring the engines-cap's
/// own `ReplySink`. It stands in for the real `EngineServer` (never
/// co-installed with it, so the shared `aether.engine` mailbox id is
/// unambiguous): on a `RouteEnvelope` it pushes the reply and the
/// `CallSettled` terminal straight back to the originating server,
/// correlation preserved, so the forwarded wire call closes the way a
/// proxy's `CallSettled` would.
#[cfg(test)]
pub(super) struct RouteInventorySink {
    reply: ListKindsResult,
    calls: Arc<AtomicUsize>,
    mailer: Arc<Mailer>,
}

#[cfg(test)]
#[actor(singleton)]
impl NativeActor for RouteInventorySink {
    type Config = RouteLoopbackConfig;
    const NAMESPACE: &'static str = "aether.engine";

    fn init(config: RouteLoopbackConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            reply: config.reply,
            calls: config.calls,
            // Cached like the real engines cap does (its `on_route`
            // propagates the inbound reply-to, which `NativeCtx` sends
            // would overwrite with this cap as sender).
            mailer: ctx.mailer(),
        })
    }

    #[handler::single]
    fn on_route(&mut self, ctx: &mut NativeCtx<'_>, _mail: RouteEnvelope) {
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        self.calls.fetch_add(1, Ordering::Relaxed);
        let reply_to = ctx.reply_target();
        // A routed call always carries a Component reply-to (the
        // originating server); without one there's nowhere to stream to.
        let SourceAddr::Component(target) = reply_to.addr else {
            return;
        };
        let correlation = reply_to.correlation_id;

        // ReplyEvent: the canned live vocabulary. The server matches it to
        // the in-flight wire call by the preserved correlation.
        self.mailer.push(
            Mail::new(
                target,
                <ListKindsResult as Kind>::ID,
                self.reply.encode_into_bytes(),
                1,
            )
            .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
        // ReplyEnd: a forwarded call has no local chain to settle, so the
        // server's `engine = Some` path waits on this explicit terminal
        // (in production the proxy lifts the substrate's `ReplyEnd` into
        // it). Pushed after the reply so the server writes the ReplyEvent
        // frame first, then closes on the CallSettled.
        self.mailer.push(
            Mail::new(
                target,
                <CallSettled as Kind>::ID,
                CallSettled::Ok.encode_into_bytes(),
                1,
            )
            .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
    }
}
