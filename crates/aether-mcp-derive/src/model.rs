//! What the parser hands the emitter.
//!
//! Every type here is settled by the time emission starts: names are minted,
//! signatures are checked, and mappings are grouped by reply kind. The emitter
//! reads these and never re-inspects the author's tokens, so a diagnostic that
//! can be spanned at all is spanned during parsing.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, quote};
use syn::{Attribute, FnArg, Ident, LitStr, Type};

/// Where a generated handler finds the actor's state, and how it calls back
/// into the retained method.
///
/// The two forms differ in one thing — whether the state arrives as a `self`
/// receiver or as a named split-capability parameter — so both the glue's first
/// parameter and the call it makes are asked of this type rather than
/// re-matched at each emission site.
#[derive(Clone)]
pub enum Host {
    /// A `self` receiver, copied verbatim onto the generated handler.
    Receiver(Box<FnArg>),
    /// `state: &mut Self::State` on a split native capability. The generated
    /// handler binds its own parameter of the same type rather than reusing the
    /// author's name, which is commonly `_state`.
    Split(Box<Type>),
}

impl Host {
    /// The generated handler's first parameter.
    pub fn parameter(&self) -> TokenStream2 {
        match self {
            Self::Receiver(receiver) => receiver.to_token_stream(),
            Self::Split(state) => quote!(__aether_state: #state),
        }
    }

    /// The state parameter a synthesized `wire` takes.
    ///
    /// A split capability's `wire` receives the same state type its handlers
    /// do, but a synthesized body only registers, so the binding is underscored
    /// to keep the hook warning-free.
    pub fn parameter_for_lifecycle(&self) -> TokenStream2 {
        match self {
            Self::Receiver(receiver) => receiver.to_token_stream(),
            Self::Split(state) => quote!(_state: #state),
        }
    }

    /// A call back into `method`, with `arguments` following the state.
    pub fn call(&self, method: &Ident, arguments: &TokenStream2) -> TokenStream2 {
        match self {
            Self::Receiver(_) => quote!(self.#method(#arguments)),
            Self::Split(_) => quote!(Self::#method(__aether_state, #arguments)),
        }
    }
}

/// The four advertised safety hints.
///
/// Held as the protocol's own four booleans because that is what the wire
/// carries; the attribute's spelling (`read_only`, `closed_world`) is a surface
/// over them, resolved once during parsing.
// The protocol defines exactly these four annotation fields. Folding them into
// an enum would invent a vocabulary neither the wire nor the attribute has.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
pub struct Hints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

impl Default for Hints {
    /// The protocol's own defaults — the conservative reading of a tool that
    /// declared nothing.
    fn default() -> Self {
        Self { read_only: false, destructive: true, idempotent: false, open_world: true }
    }
}

/// The literal metadata one `#[mcp::tool]` carries into its registration.
pub struct Metadata {
    /// The validated public tool name, also pasted into the minted kind name.
    pub name: String,
    /// The span of the `name` literal, so a name-derived diagnostic points at
    /// what the author wrote rather than at the whole attribute.
    pub name_span: Span,
    pub title: Option<LitStr>,
    pub description: LitStr,
    pub hints: Hints,
}

/// One `#[mcp::tool]` method and everything minted for it.
pub struct Tool {
    /// The retained method, which stays an ordinary helper.
    pub method: Ident,
    pub metadata: Metadata,
    pub host: Host,
    /// The transport context `C` from `mcp::Context<'_, C>`.
    pub ctx: Type,
    pub input: Type,
    pub output: Type,
    /// True when the method returns `mcp::Outcome<Output>` and may defer.
    /// A synchronous `Result` tool answers inside its dispatcher and takes no
    /// reply mapping.
    pub deferred: bool,
    /// `#[doc(hidden)] pub` sibling: the hidden `{ input }` request kind.
    pub request_struct: Ident,
    /// Private sibling: the `{ output }` value wrapper.
    pub value_struct: Ident,
    /// Private sibling: the `{ inline, addressed }` boundary output.
    pub boundary_struct: Ident,
    /// `"{NAMESPACE}.tool.{name}"`.
    pub kind_name: LitStr,
    /// The generated dispatcher's name.
    pub dispatch_name: Ident,
    pub docs: Vec<Attribute>,
}

/// How one tool's declared output is produced from a downstream reply.
pub enum Mapper {
    /// `#[mcp::reply(K, tool = t)]` written on the mapping method itself: it
    /// takes the state, the transport context, and the owned reply. The
    /// transport type is not carried here — the group's own fallback states it,
    /// and every mapping in one group shares it.
    Standalone { method: Ident, host: Host },
    /// `#[mcp::reply(K, tool = t, map = m)]` written above a retained handler:
    /// `m` is a state-free `fn(&K) -> Result<Output, ToolError>`, so the
    /// generated branch can borrow the reply and leave the owned value to the
    /// fallback.
    Branch { method: Ident },
}

/// One tool's branch inside a reply kind's handler.
pub struct Mapping {
    /// The `#[mcp::tool]` method this branch answers for.
    pub tool: Ident,
    pub mapper: Mapper,
    /// The marker's span, for a diagnostic about this mapping alone.
    pub span: Span,
}

/// What runs when no tool branch claimed the reply.
pub enum Fallback {
    /// Nothing did before and nothing does now: the group owns a fresh handler
    /// made only of tool branches.
    Vacant { host: Host, ctx: Type },
    /// The author's own `#[handler::manual]` method keeps the handler slot and
    /// the branches are injected at the top of its body, addressed to the
    /// bindings that method already names.
    Manual { method: Ident, ctx_ident: Ident, reply_ident: Ident },
    /// The author's `#[http::reply]` mapper, retained as a plain helper. The
    /// generated handler calls it and then `http::answer_deferred`.
    Http { method: Ident, host: Host, ctx: Type },
}

/// Every mapping that answers one reply kind, plus what happens when none of
/// them does.
///
/// One group is one actor handler, which is the invariant the whole reply
/// surface exists to preserve: a reply kind that serves three tools, an HTTP
/// route, and an authored correlation still has exactly one `#[handler]`.
pub struct ReplyGroup {
    pub kind: Type,
    pub mappings: Vec<Mapping>,
    pub fallback: Fallback,
    /// The generated handler's name, derived from the reply kind so several
    /// tool mappings share it.
    pub handler_name: Ident,
}

impl ReplyGroup {
    /// The grouping key: the reply kind's token spelling.
    ///
    /// Two markers name one group when they spell the kind identically. A
    /// macro cannot resolve `QueryResult` and `crate::QueryResult` to the same
    /// type, so the spelling is the only key available; writing one kind two
    /// ways yields two groups and the duplicate-handler collision that follows
    /// is an ordinary actor-level compile error.
    pub fn key(kind: &Type) -> String {
        kind.to_token_stream().to_string()
    }
}
