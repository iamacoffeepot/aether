//! `bundle_programs` export generator: a macro hook over host-side proc-macro codegen.
//!
//! `export!` collects framework-owned descriptor envelopes into `actors` and the
//! listed types into `exports`, then invokes
//! `$gen!(@aether_export_generate { remaining_generators } { boot, default, actors, exports })`.
//! This macro is that hook. It forwards into `__program_export_generate`, which
//! selects the `aether_bloomery_program` extension on exported paths, emits the
//! hidden bundle root and inline invocation child, rewrites `exports` only, and
//! appends a root envelope to `actors`. Original program envelopes stay attached
//! to their types. Macros cannot reflect on target-crate trait impls, so there is
//! no empty host-side generator trait. The derive crate is `proc-macro = true`
//! and is not linked into guest wasm.

/// Framework-owned mailbox namespace of the generated program bundle root.
///
/// One root is emitted per `export!(…, generators = [bundle_programs])`.
/// Load it with this selector. Invocation children are inline and are not
/// module exports. The root is never a `boot` actor.
pub const PROGRAM_NAMESPACE: &str = "aether.bloomery.program";

/// Export generator for bloomery programs.
///
/// Exact author form:
///
/// ```ignore
/// aether_actor::export!(
///     Summarize,
///     Refuse,
///     generators = [aether_bloomery_program::bundle_programs],
/// );
/// ```
///
/// The generator reads `actors` envelopes for the current `exports` paths,
/// keeps ordinary actors in the export selection, and replaces selected
/// programs with one hidden bundle root. Program types remain in
/// `actors` with their own extensions; those extensions are not copied onto
/// the root. Zero programs, a `default` that is a program,
/// reserved/duplicate `NAME`, and missing companions are compile errors
/// — entries are never dropped silently. `program` already names the
/// attribute macro, so this generator is not `program!`.
#[macro_export]
macro_rules! bundle_programs {
    (@aether_export_generate
        { remaining_generators: [$($rest:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [$($actors:tt)*], exports: [$($exports:tt)*] }
    ) => {
        $crate::__program_export_generate! {
            remaining_generators: [$($rest),*]
            boot: $boot
            default: $default
            actors: [$($actors)*]
            exports: [$($exports)*]
        }
    };
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "bundle_programs is an export! generator; write `export!(..., generators = [bundle_programs])`"
        );
    };
}
