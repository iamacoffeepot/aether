//! `export_asset!` expansion (ADR-0163 §2): embed an asset file in a
//! wasm custom section named `aether.asset.<path>`.
//!
//! The emitted static rides the same `#[unsafe(link_section)]` path as the
//! `aether.kinds` sections (ADR-0028 / ADR-0032): the bytes land in a
//! custom section keyed by the asset path, which is the section the
//! ADR-0163 host-side indexer (#3969) reads at load. The asset is not
//! addressable from guest code — no symbol the guest can name resolves to
//! it.
//!
//! The bytes reach linear memory only if something pins the static against
//! wasm-ld's default `--gc-sections`, and this expansion deliberately does
//! not. On a wasm target rustc handles a `#[link_section]` static by
//! copying its initializer bytes into `wasm.custom_sections` module
//! metadata (which the LLVM backend emits as the named custom section)
//! while leaving the static itself an ordinary linear-memory global with
//! no section attribute — two copies from codegen onward. The custom
//! section is unconditional: rustc emits the metadata whenever it codegens
//! the static, and wasm-ld copies custom sections from every linked object.
//! The linear-memory copy is a dead internal global, so LLVM GlobalDCE (in
//! release) and wasm-ld `--gc-sections` (in debug) drop it before it
//! reaches the shipped wasm. Omitting `#[used]` — which would pin the dead
//! global as a no-strip symbol — is what delivers ADR-0163 §2's "never
//! instantiated into linear memory" from the macro alone, with no
//! post-build strip or `wasm-opt` pass. The `asset_sections` tripwire test
//! asserts the payload appears in no data segment, so a toolchain shift or
//! a `-C link-dead-code` / `--no-gc-sections` link run that resurrects the
//! copy reds immediately.
//!
//! On non-wasm targets the expansion reduces to an anonymous
//! `include_bytes!` const: no section is emitted (native object
//! formats reject long section names — Mach-O caps them at 16 bytes),
//! but a missing or misspelled path still fails every native
//! `cargo check` / clippy build, not just the wasm cross-build.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::LitStr;

/// FNV-1a over the path string — a stable, dependency-free source of
/// ident uniqueness for the emitted statics. Two invocations with the
/// same path in one module collide on the ident and fail loudly at
/// compile time, which is the desired behavior (the path is the
/// asset's lookup key).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn expand_export_asset(path_lit: &LitStr) -> syn::Result<TokenStream2> {
    let path = path_lit.value();
    if path.is_empty() {
        return Err(syn::Error::new(path_lit.span(), "export_asset! takes a non-empty file path"));
    }

    let section_name = format!("aether.asset.{path}");
    let hash = fnv1a(path.as_bytes());
    let asset_ident = format_ident!("__AETHER_ASSET_{hash:016X}");
    let guard_ident = format_ident!("__AETHER_ASSET_GUARD_{hash:016X}");

    // The interpolated `#path_lit` keeps the caller's span, so
    // `include_bytes!` resolves the path relative to the invoking source
    // file — the same semantics as writing `include_bytes!` by hand. This
    // is the exact `static [u8; N] = *include_bytes!(…)` form ADR-0163 §2
    // specifies; it mirrors the `aether.kinds` section statics.
    //
    // The guard static exports a symbol named exactly like the section.
    // Same-named custom sections from separate statics are silently
    // concatenated by wasm-ld (the aether.kinds sections rely on that),
    // which for assets would corrupt the payload; the duplicated export
    // name turns that case — the same path exported twice across modules
    // or crates, or two crates shipping distinct assets under one path —
    // into a duplicate-symbol link error.
    Ok(quote! {
        #[cfg(target_family = "wasm")]
        #[doc(hidden)]
        #[unsafe(link_section = #section_name)]
        static #asset_ident: [u8; ::core::include_bytes!(#path_lit).len()] =
            *::core::include_bytes!(#path_lit);

        #[cfg(target_family = "wasm")]
        #[doc(hidden)]
        #[unsafe(export_name = #section_name)]
        static #guard_ident: u8 = 0;

        #[cfg(not(target_family = "wasm"))]
        const _: &[u8] = ::core::include_bytes!(#path_lit);
    })
}
