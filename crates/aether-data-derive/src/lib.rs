// The deny-list visitor builds a flat scan over the body's expression
// paths; `if let Some(..)` over the matched-prefix branch reads clearer
// than `map_or_else`. Allow at the crate root because cargo doesn't
// permit `[lints.clippy]` overrides alongside `lints.workspace = true`
// in the manifest (mirrors `aether-actor-derive`).
#![allow(clippy::option_if_let_else)]

//! Proc-macro home for the `#[transform]` attribute (ADR-0048 §1).
//!
//! A transform is a **data-layer primitive** — a pure `Kind -> Kind`
//! function with zero dependence on the actor framework. Its runtime
//! types ([`TransformEntry`](aether_data::TransformEntry),
//! [`TransformError`](aether_data::TransformError), the link-time
//! inventory) live in `aether-data`; this crate is the sibling
//! proc-macro that `aether-data` cannot itself be (`proc-macro = true`
//! forbids exporting runtime items). `aether-data` re-exports the macro
//! as `aether_data::transform` behind the `derive` feature.
//!
//! The macro's three ADR-0048 §1 responsibilities:
//!
//! 1. **Stable name-based `transform_id`.**
//!    `fnv1a_64(TRANSFORM_DOMAIN ++ "{crate}::{module_path}::{fn}")`,
//!    tagged `Tag::Transform`. Built at the *consumer's* compile time
//!    from `concat!(env!("CARGO_PKG_NAME"), "::", module_path!(), "::",
//!    fn)` so identity tracks the fully-qualified name, not the
//!    position in the file.
//! 2. **Deny-list purity scan.** Walks the body's expression paths and
//!    rejects host-fn imports, handler-context types, the sync
//!    request/reply primitive, and compile-time-catchable
//!    nondeterminism sources (`std::env`, `std::time`, `core::time`).
//!    Best-effort: it sees only the immediate body, not helper-fn
//!    bodies, and there is no runtime sandbox (ADR-0048
//!    Consequences/Negative). First-party review is the other defense.
//! 3. **Link-time inventory submission.** Emits an `inventory::submit!`
//!    of a `TransformEntry` carrying the id, input/output kind ids, the
//!    name, and a type-erased `invoke` thunk that decodes each input
//!    slice, calls the user fn, and encodes the output.
//!
//! There is no FFI shim, no `extern "C"`, no custom section — the
//! original wasm-export design was deferred (ADR-0048 revision
//! 2026-05-20).

use core::iter;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, FnArg, ItemFn, ReturnType, Type, parse_macro_input};

/// ADR-0048 §1 cap on input parameters.
const MAX_TRANSFORM_INPUTS: usize = 8;

/// `#[transform]` — register a pure `Kind -> Kind` function as a native
/// transform (ADR-0048 §1). The annotated fn is left intact (so it
/// stays unit-testable as an ordinary fn) and gains a link-time
/// inventory entry the substrate's `TransformRegistry` collects at
/// startup.
///
/// See the crate docs for the three responsibilities (id derivation,
/// purity scan, inventory submission). The macro emits `compile_error!`
/// for a non-fn item, a `self` receiver, generics, a 9th input
/// parameter, a missing return type, or a body that names a denied
/// path.
#[proc_macro_attribute]
pub fn transform(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand_transform(&func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_transform(func: &ItemFn) -> syn::Result<TokenStream2> {
    let (input_types, output_type) = validate_signature(func)?;

    // Deny-list purity scan over the immediate body (ADR-0048 §1). Best
    // effort — helper-fn bodies aren't visible here.
    purity_scan(&func.block)?;

    let inventory = emit_inventory(func, &input_types, output_type);
    Ok(quote! {
        #func
        #inventory
    })
}

/// Enforce the ADR-0048 §1 signature contract: no generics, no `self`,
/// ≤ 8 inputs, and a single (non-`()`) return type. Returns the input
/// parameter types in slot order plus the output type.
fn validate_signature(func: &ItemFn) -> syn::Result<(Vec<&Type>, &Type)> {
    let sig = &func.sig;

    // No generics — transforms are monomorphic (ADR-0048 §1).
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "transforms cannot be generic -- they are monomorphic Kind -> Kind functions \
             (ADR-0048 §1)",
        ));
    }

    // Collect the input parameter types, rejecting any `self` receiver
    // and capping at 8 (ADR-0048 §1).
    let mut input_types: Vec<&Type> = Vec::new();
    for arg in &sig.inputs {
        match arg {
            FnArg::Receiver(recv) => {
                return Err(syn::Error::new(
                    recv.span(),
                    "transforms cannot take `self` -- they are free-standing pure functions \
                     (ADR-0048 §1)",
                ));
            }
            FnArg::Typed(pat) => input_types.push(&pat.ty),
        }
    }
    if input_types.len() > MAX_TRANSFORM_INPUTS {
        return Err(syn::Error::new(
            sig.inputs.span(),
            format!(
                "transforms accept at most {MAX_TRANSFORM_INPUTS} inputs (ADR-0048 §1); found {}",
                input_types.len(),
            ),
        ));
    }

    // Single return type, also a `Kind` (ADR-0048 §1). A bare `()`
    // return is rejected — a transform produces a kind value.
    let output_type: &Type = match &sig.output {
        ReturnType::Type(_, ty) => ty,
        ReturnType::Default => {
            return Err(syn::Error::new(
                sig.span(),
                "transforms must return a single Kind value (ADR-0048 §1)",
            ));
        }
    };

    Ok((input_types, output_type))
}

/// Emit the per-type `Kind` bound assertions + the link-time inventory
/// submission (id derivation, the static input-kind-id slice, and the
/// type-erased `invoke` thunk). Codegen only — the signature is already
/// validated and the body already purity-scanned.
fn emit_inventory(func: &ItemFn, input_types: &[&Type], output_type: &Type) -> TokenStream2 {
    let fn_name = &func.sig.ident;
    let fn_name_str = fn_name.to_string();

    // Per-type `Kind` bound assertions: the macro can't check trait
    // bounds at expansion time, so emit a `const _: fn() = || { <T as
    // Kind>::ID; };` per input + output. A build error fires if any type
    // doesn't impl `Kind` (ADR-0048 §1).
    let bound_assertions = input_types
        .iter()
        .chain(iter::once(&output_type))
        .map(|ty| {
            quote! {
                const _: fn() = || {
                    let _ = <#ty as ::aether_data::transform::__transform_runtime::Kind>::ID;
                };
            }
        });

    // Fully-qualified name string at the consumer's compile time:
    // `"{crate}::{module_path}::{fn}"`. `module_path!()` already begins
    // with the crate's lib/bin name as the first segment, so prefixing
    // `CARGO_PKG_NAME` keeps the id stable even when two crates share a
    // module path tail.
    let name_expr = quote! {
        ::core::concat!(
            ::core::env!("CARGO_PKG_NAME"), "::",
            ::core::module_path!(), "::",
            #fn_name_str,
        )
    };

    // The `invoke` thunk: decode each input slice (slot-index order)
    // against its declared kind via `Kind::decode_from_bytes`, call the
    // user fn, encode the output via `Kind::encode_into_bytes`. Decode
    // failure -> `InputDecode { slot }`; arity mismatch ->
    // `InputArity`. The output-byte cap is the executor's job, not the
    // thunk's.
    let arity = input_types.len();
    let decode_bindings = input_types.iter().enumerate().map(|(slot, ty)| {
        let local = format_ident!("__in{slot}");
        quote! {
            let #local: #ty = match
                <#ty as ::aether_data::transform::__transform_runtime::Kind>::decode_from_bytes(
                    __inputs[#slot],
                )
            {
                ::core::option::Option::Some(v) => v,
                ::core::option::Option::None => {
                    return ::core::result::Result::Err(
                        ::aether_data::transform::__transform_runtime::TransformError::InputDecode {
                            slot: #slot,
                        },
                    );
                }
            };
        }
    });
    let decode_locals = (0..arity).map(|slot| format_ident!("__in{slot}"));

    let entry_static = format_ident!("__AETHER_TRANSFORM_ENTRY_{}", fn_name_str.to_uppercase());

    // Static slices the inventory entry borrows. `inventory::submit!`
    // needs const-constructible borrows, so the input-kind-id list is a
    // file-scoped `static` array rather than an inline literal.
    let input_kinds_static =
        format_ident!("__AETHER_TRANSFORM_INPUTS_{}", fn_name_str.to_uppercase());
    let input_kind_exprs = input_types.iter().map(|ty| {
        quote! {
            <#ty as ::aether_data::transform::__transform_runtime::Kind>::ID
        }
    });

    quote! {
        #(#bound_assertions)*

        // Link-time inventory submission (ADR-0048 §1). Cfg-gated to
        // non-wasm targets because `inventory` doesn't link on
        // `wasm32-unknown-unknown` (same gate as the Kind derive's
        // descriptor inventory).
        #[cfg(not(target_arch = "wasm32"))]
        const _: () = {
            static #input_kinds_static:
                [::aether_data::transform::__transform_runtime::KindId; #arity] = [
                    #(#input_kind_exprs),*
                ];

            fn #entry_static(__inputs: &[&[u8]])
                -> ::core::result::Result<
                    ::aether_data::transform::__transform_runtime::Vec<u8>,
                    ::aether_data::transform::__transform_runtime::TransformError,
                >
            {
                if __inputs.len() != #arity {
                    return ::core::result::Result::Err(
                        ::aether_data::transform::__transform_runtime::TransformError::InputArity {
                            expected: #arity,
                            actual: __inputs.len(),
                        },
                    );
                }
                #(#decode_bindings)*
                let __out: #output_type = #fn_name(#(#decode_locals),*);
                ::core::result::Result::Ok(
                    <#output_type as ::aether_data::transform::__transform_runtime::Kind>::encode_into_bytes(
                        &__out,
                    ),
                )
            }

            ::aether_data::transform::__transform_runtime::inventory::submit! {
                ::aether_data::transform::__transform_runtime::TransformEntry {
                    transform_id:
                        ::aether_data::transform::__transform_runtime::TransformId(
                            ::aether_data::with_tag(
                                ::aether_data::Tag::Transform,
                                ::aether_data::fnv1a_64_prefixed(
                                    &::aether_data::TRANSFORM_DOMAIN,
                                    #name_expr.as_bytes(),
                                ),
                            ),
                        ),
                    input_kind_ids: &#input_kinds_static,
                    output_kind_id:
                        <#output_type as ::aether_data::transform::__transform_runtime::Kind>::ID,
                    name: #name_expr,
                    invoke: #entry_static
                        as ::aether_data::transform::__transform_runtime::InvokeFn,
                }
            }
        };
    }
}

/// Walk a function body's expression paths and reject any that name a
/// denied path (ADR-0048 §1 deny-list). Returns the first violation as
/// a span-located `compile_error!`.
fn purity_scan(block: &syn::Block) -> syn::Result<()> {
    let mut scanner = PurityScanner { violation: None };
    scanner.visit_block(block);
    match scanner.violation {
        Some(span) => Err(syn::Error::new(
            span,
            "transforms cannot call host functions or access handler context -- see ADR-0048",
        )),
        None => Ok(()),
    }
}

/// One denied path: a sequence of `::`-joined segment tails. A body path
/// matches if its trailing segments end with this sequence (so both
/// `aether::send_mail_p32` and a `use`-shortened `send_mail_p32` are
/// caught for the single-segment entries, and qualified `std::time::*`
/// is caught by the two-segment prefix entries).
struct DeniedPath {
    /// Segments to match against the *trailing* run of a body path.
    tail: &'static [&'static str],
}

/// The deny-list (ADR-0048 §1):
/// - host-fn imports (`aether::send_mail_p32`, `reply_mail_p32`,
///   `resolve_*_p32`, and the other SDK host fns),
/// - handler-context types (`aether_actor::Ctx`, `MailCtx`),
/// - the sync request/reply primitive (`aether_actor::wait_reply`),
/// - compile-time-catchable nondeterminism (`std::env::*`,
///   `std::time::*`, `core::time::*`).
const DENY_LIST: &[DeniedPath] = &[
    // Host fns — match the bare fn tail so both qualified and
    // use-shortened call sites are caught.
    DeniedPath {
        tail: &["send_mail_p32"],
    },
    DeniedPath {
        tail: &["reply_mail_p32"],
    },
    DeniedPath {
        tail: &["send_mail_traced_p32"],
    },
    DeniedPath {
        tail: &["save_state_p32"],
    },
    DeniedPath {
        tail: &["resolve_mailbox_p32"],
    },
    DeniedPath {
        tail: &["resolve_kind_p32"],
    },
    DeniedPath {
        tail: &["wait_reply"],
    },
    // Handler-context types.
    DeniedPath {
        tail: &["aether_actor", "Ctx"],
    },
    DeniedPath {
        tail: &["aether_actor", "MailCtx"],
    },
    // Nondeterminism sources, by two-segment prefix so any item under
    // them (`now`, `Instant`, `var`, etc.) is rejected.
    DeniedPath {
        tail: &["std", "env"],
    },
    DeniedPath {
        tail: &["std", "time"],
    },
    DeniedPath {
        tail: &["core", "time"],
    },
];

/// Body-path collector + matcher. Records the span of the first path
/// whose trailing segments match a deny-list entry.
struct PurityScanner {
    violation: Option<proc_macro2::Span>,
}

impl PurityScanner {
    /// Check one path's segment idents against the deny-list. A
    /// deny-entry matches if the path's trailing segments equal the
    /// entry's `tail` sequence (so `std::time::Instant::now` matches the
    /// `["std", "time"]` entry, and a use-shortened `send_mail_p32`
    /// matches the single-segment `["send_mail_p32"]` entry).
    fn check_path(&mut self, path: &syn::Path) {
        if self.violation.is_some() {
            return;
        }
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        for denied in DENY_LIST {
            // Single-segment fn entries match the fn ident anywhere in
            // the path (catches both `aether::send_mail_p32` and a
            // `use`-shortened `send_mail_p32`). Multi-segment prefix
            // entries (the `std::time` / `core::time` / `std::env`
            // nondeterminism roots, plus the `aether_actor::Ctx` types)
            // anchor at the path head so any item beneath them is
            // rejected.
            let matched = if denied.tail.len() == 1 {
                segs.iter().any(|s| s == denied.tail[0])
            } else {
                segs.len() >= denied.tail.len() && segs[..denied.tail.len()] == *denied.tail
            };
            if matched {
                self.violation = Some(path.span());
                return;
            }
        }
    }
}

impl<'ast> Visit<'ast> for PurityScanner {
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        self.check_path(&node.path);
        visit::visit_expr_path(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        // A call's callee is an `Expr::Path` for free-fn / path calls;
        // `visit_expr_path` handles it. Method calls (`x.foo()`) carry
        // no callee path, so a `std::time::Instant::now()` written as a
        // path-call is the case that matters here — already covered.
        if let Expr::Path(p) = &*node.func {
            self.check_path(&p.path);
        }
        visit::visit_expr_call(self, node);
    }
}
