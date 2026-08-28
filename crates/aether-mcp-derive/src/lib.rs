//! Proc macros for the tool-authoring surface over the `aether.mcp.server`
//! capability. Three attribute macros, re-exported through `aether_mcp` so a
//! provider writes `#[mcp::router]` / `#[mcp::tool]` / `#[mcp::reply]` beside
//! the `mcp::Context` / `mcp::Outcome` / `mcp::ToolError` runtime types the
//! parent crate owns.
//!
//! A tool is a route. `#[mcp::router]` therefore follows `#[http::router]`
//! step for step: it mints a real wrapper kind per tool, emits handler glue,
//! and injects a reflexive host-stamped registration into `wire`. What it adds
//! over the HTTP macro is the composite reply handler, and that exists because
//! a tool's answer can arrive on a reply kind that several tools — and an HTTP
//! route, and an authored correlation — already share.
//!
//! ## Attribute order
//!
//! On an actor that also owns HTTP routes the required outer-first order is:
//!
//! ```text
//! #[mcp::router]
//! #[http::router]
//! #[runtime]
//! impl NativeActor for MyCapability { … }
//! ```
//!
//! Attribute macros expand outermost first, and this one has to run before
//! `#[http::router]`: composing tool branches onto an existing `#[http::reply]`
//! mapper means consuming that marker and emitting *one* handler for the reply
//! kind. Expanded second, it would find the marker gone and a second handler
//! already emitted. `#[mcp::router]` detects that case and says so rather than
//! silently skipping the composition.
//!
//! ## What the generated code emits
//!
//! Per tool, three sibling types and one dispatcher; per reply kind, one
//! handler; per impl, one registration send per tool. The emitted paths are
//! absolute (`::aether_mcp`, `::aether_data`, `::aether_actor`, `::aether_http`,
//! `::serde`), so this crate names none of those types and depends on nothing
//! but the macro toolkit.
//!
//! ## Tool input and output types must not use serde renames
//!
//! **This is a real constraint the macro cannot check.** `aether-data-derive`
//! emits `EnumVariant::name` from the Rust identifier and does not inspect
//! `#[serde(rename)]` or `#[serde(rename_all)]`. The protocol translator trusts
//! `SchemaType`, and the JSON-to-wire edge uses `encode_schema` against that
//! same tree, so a renamed field or variant would be advertised to a client
//! under one spelling and encoded under another.
//!
//! A tool's `Input` and `Output` are named in the method signature; their
//! declarations are elsewhere, and a proc macro sees only the tokens handed to
//! it. `#[mcp::router]` therefore cannot see a rename attribute on a type it
//! processes and emits no diagnostic for one. Until that mismatch is resolved
//! in `aether-data-derive`, avoid serde field and variant renames on every type
//! reachable from a tool's `Input` or `Output`.

mod emit;
mod model;
mod naming;
mod parse;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::{Ident, ItemImpl, parse_macro_input};

/// `#[mcp::tool(name = …, description = …, …)]` — a marker consumed by
/// `#[mcp::router]` on the enclosing impl.
///
/// Reaching this expansion means the impl is missing `#[mcp::router]`, and the
/// emitted `compile_error!` says so. Under correct usage `router` strips the
/// marker before the compiler resolves it, so this body never runs.
///
/// Requires literal `name` and `description`. `name` matches
/// `^[a-z][a-z0-9_]{0,63}$`, because it is pasted verbatim into the minted kind
/// name `"{NAMESPACE}.tool.{name}"`. `description` is 1 through 4,096 UTF-8
/// bytes and the optional `title` is 1 through 256.
///
/// The bare flags `read_only`, `destructive`, `non_destructive`, `idempotent`,
/// `open_world`, and `closed_world` lower to the four advertised annotation
/// hints, all four of which are always emitted. Their defaults are the
/// protocol's own — not read-only, destructive, not idempotent, open-world —
/// and `read_only` also selects a non-destructive hint. Stating two flags that
/// contradict each other is a compile error.
///
/// On a split native capability the state parameter must name its state type
/// concretely (`state: &mut MyCapabilityState`), not as `Self::State`. A tool
/// method is retained as a plain helper, and `#[actor]` lifts helpers out of
/// the trait impl into an inherent one where the trait's associated type no
/// longer resolves. `#[http::route]` methods carry the same constraint for the
/// same reason.
///
/// The method's `Input` and `Output` must implement `aether_data::Schema` plus
/// serde's `Serialize` and `Deserialize`, and **must not carry serde renames**
/// — see the crate-level documentation for why the macro cannot check that.
#[proc_macro_attribute]
pub fn tool(_args: TokenStream, item: TokenStream) -> TokenStream {
    orphaned_marker("tool", item)
}

/// `#[mcp::reply(ReplyKind, tool = …, map = …)]` — a marker consumed by
/// `#[mcp::router]`, stackable when one reply kind answers several tools.
///
/// Without `map =`, the annotated method *is* the mapping: it takes the state,
/// `&mut NativeCtx<'_, Manual>`, and the owned reply, and returns
/// `Result<Output, mcp::ToolError>`.
///
/// With `map =`, the annotated method is the handler that already owns the
/// reply kind — an authored `#[handler::manual]` or an `#[http::reply]` mapper
/// — and `map` names a state-free `fn(&ReplyKind) -> Result<Output,
/// mcp::ToolError>`. Keeping the branch mapper state-free is what lets the
/// generated handler borrow the reply for the selected tool and leave the owned
/// value to the fallback.
///
/// When the `map =` form annotates an authored `#[handler::manual]`, the
/// branches are injected at the top of that method and read its context and
/// reply bindings, so neither may be underscore-named any more — rename
/// `_result` to `result` if the handler previously ignored it.
///
/// Like `#[mcp::tool]`, reaching this expansion means the enclosing impl is
/// missing `#[mcp::router]`.
#[proc_macro_attribute]
pub fn reply(_args: TokenStream, item: TokenStream) -> TokenStream {
    orphaned_marker("reply", item)
}

/// The shared fallback body for both markers: state the missing router and
/// re-emit the item, so the author sees one pointed error rather than that
/// error plus every consequence of the method disappearing.
fn orphaned_marker(marker: &str, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    let message = format!(
        "#[mcp::{marker}] requires #[mcp::router] on the enclosing impl block \
         (written above #[http::router] and #[runtime])"
    );
    quote_spanned! { item.span() =>
        ::core::compile_error!(#message);
        #item
    }
    .into()
}

/// `#[mcp::router]` — the impl-block attribute that expands the tool-authoring
/// surface.
///
/// Takes no arguments (exclusive registration: one actor claims each tool
/// name), or the bare ident `shared`, which registers every tool on the impl
/// with `shared: true` so N interchangeable instances join one round-robin
/// member set. A shared set admits a member only when every descriptor byte and
/// every piece of metadata matches, which one impl expanded N times satisfies
/// by construction.
#[proc_macro_attribute]
pub fn router(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(item as ItemImpl);
    let shared = match sharing(TokenStream2::from(args)) {
        Ok(shared) => shared,
        Err(error) => return error.into_compile_error().into(),
    };
    match parse::router(&mut item, shared) {
        Ok(router) => emit::router(item, &router).into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Read `#[mcp::router(...)]`'s optional argument.
fn sharing(args: TokenStream2) -> syn::Result<bool> {
    if args.is_empty() {
        return Ok(false);
    }
    match syn::parse2::<Ident>(args.clone()) {
        Ok(ident) if ident == "shared" => Ok(true),
        _ => Err(syn::Error::new_spanned(
            args,
            "#[mcp::router] accepts no arguments, or the bare ident `shared` for a round-robin member set",
        )),
    }
}

// The expansion itself — kind minting, dispatcher shape, composite reply
// routing, registration content — is exercised through the compiled fixture
// actor in `aether-mcp`'s runtime tests, which boots the real capability and
// drives a synchronous tool, two deferred tools sharing one reply kind, and an
// HTTP route through one actor. Asserting over token output here would only
// restate the `quote!` blocks. The parser's refusals carry the `tests/ui`
// suite, and the two identifier folds carry the tripwires beside them.
#[cfg(test)]
mod tests {
    use super::sharing;
    use quote::quote;

    // Tripwire: `shared` is the only accepted argument, and mis-reading it
    // would flip every tool on an impl between an exclusive claim and a
    // round-robin join — a change with no local symptom, since a sole member
    // of a shared set behaves exactly like an exclusive holder until a second
    // instance registers.
    #[test]
    fn sharing_accepts_only_the_bare_opt_in() {
        assert!(!sharing(quote!()).expect("no argument is the exclusive claim"));
        assert!(sharing(quote!(shared)).expect("the bare ident opts in"));
        assert!(sharing(quote!(exclusive)).is_err());
        assert!(sharing(quote!(shared = true)).is_err());
    }
}
