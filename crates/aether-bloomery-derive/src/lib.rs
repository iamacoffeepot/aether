//! Proc macros for bloomery authoring, one module per macro family.
//!
//! - `view`: `#[view]` / `#[fold]`, fixed aggregate views with typed fold
//!   methods.
//! - `reactor`: `#[reactor]` / `#[rule]`, signature-based reactors.
//! - `program`: `#[program]`, program authoring.
//! - `bundle`: `__bundle_export_generate`, the host-side codegen behind the
//!   `bundle` export generator.
//!
//! Every expansion names paths in `aether_bloomery_program`, which re-exports
//! these macros so an author depends on one crate.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;

mod bundle;
mod program;
mod reactor;
mod view;

/// Generate an ordinary View implementation from typed #[fold] methods on a
/// fixed aggregate.
#[proc_macro_attribute]
pub fn view(attr: TokenStream, item: TokenStream) -> TokenStream {
    view::view_attribute(attr, item)
}

/// Marker consumed by the view macro.
#[proc_macro_attribute]
pub fn fold(_attr: TokenStream, item: TokenStream) -> TokenStream {
    view::fold_attribute(item)
}

/// Outer attribute on `impl Reactor for Name`. Consumes `#[rule]` methods
/// and generates preparation/evaluation plus associated-type visitors.
///
/// Takes no arguments. The impl must declare `const NAMESPACE` and at least one
/// `#[rule]`. The reactor type must be a unit struct. Each rule takes `&self`,
/// a typed trigger, then owned view or guard parameters, and returns exactly
/// one mail-capable output.
#[proc_macro_attribute]
pub fn reactor(attr: TokenStream, item: TokenStream) -> TokenStream {
    reactor::reactor_attribute(&attr, item)
}

/// Marker consumed by [`macro@reactor`]. Reaching this expansion means the
/// enclosing impl is missing `#[reactor]`.
#[proc_macro_attribute]
pub fn rule(_attr: TokenStream, item: TokenStream) -> TokenStream {
    reactor::rule_attribute(item)
}

/// Outer attribute on `impl Program for Name`.
///
/// Takes no arguments. The impl must carry a `///` doc, the program's tool
/// description, which `#[program]` writes as `const DOC`, and must declare
/// `NAME`, `INTENT`, `MODE` (`Pure` or `Sampled`), `Input`, and `Result`.
/// `Input` must be a struct with a `///` doc on every field and variant it
/// exposes; the first gap is a compile error naming it. `run` is an associated function: no
/// receiver. `fn run` pairs with `Env<Sync>`; `async fn run` pairs with
/// `Env<Async>`. Optional cap bindings follow `env`, each an `Http`, a
/// `Process`, or a `Workspace` in whatever path the author writes it; each is
/// Sampled, so it requires `Mode::Sampled`. The export descriptor
/// carries the canonical name, and a check at the parameter confirms the type
/// is the API that name selects.
#[proc_macro_attribute]
pub fn program(attr: TokenStream, item: TokenStream) -> TokenStream {
    program::program_attribute(&attr, item)
}

/// Hidden `bundle` export generator. Invoked by the runtime crate's `bundle!` hook, not by authors.
#[doc(hidden)]
#[proc_macro]
pub fn __bundle_export_generate(input: TokenStream) -> TokenStream {
    bundle::generate(input)
}
