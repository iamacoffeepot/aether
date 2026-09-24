//! `bundle` export generator: a macro hook over host-side proc-macro codegen.
//!
//! `export!` collects framework-owned descriptor envelopes into `actors` and the
//! listed types into `exports`, then invokes
//! `$gen!(@aether_export_generate { remaining_generators } { boot, default, actors, exports, private })`.
//! This macro is that hook. It forwards into `__bundle_export_generate`, which
//! selects the `aether_bloomery_program` and `aether_bloomery_reactor`
//! extensions on exported paths, emits one hidden bundle root with the state,
//! handlers, and sections of the roles present, rewrites `exports`, appends a
//! root envelope to `actors`, and appends its per-call invocation actor to
//! `private` when programs are present. Original program and reactor envelopes
//! stay attached to their types. Macros cannot reflect on target-crate trait
//! impls, so there is no empty host-side generator trait. The derive crate is
//! `proc-macro = true` and is not linked into guest wasm.

/// Export generator for bloomery bundles: programs, reactors, or both.
///
/// Exact author form:
///
/// ```ignore
/// aether_actor::export!(
///     Summarize,
///     SourcePublisher,
///     generators = [aether_bloomery_bundle::bundle],
/// );
/// ```
///
/// The generator reads `actors` envelopes for the current `exports` paths,
/// keeps ordinary actors in the export selection, and replaces every selected
/// program and reactor with one hidden bundle root at [`crate::BUNDLE_NAMESPACE`],
/// at the first one's position. Program and reactor types remain in `actors`
/// with their own extensions; those extensions are not copied onto the root.
/// The root carries handlers, state, and a custom section only for the roles
/// present: `Invoke` / `Invoked` through a per-seq inline child plus
/// `aether.bloomery.programs` for programs, `Warm` / `Event` / `StatusQuery`
/// plus `aether.bloomery.reactors` for reactors. The root is never a `boot`
/// actor. With programs present, the generator appends the invocation actor
/// to the pipeline's `private` list, beside any the author wrote, so
/// `export!` marks it rebuildable and a replace rebuilds it like any other
/// private inline child. These are compile errors — entries are never dropped silently:
/// - a module with neither a `#[program]` nor a `#[reactor]`;
/// - a duplicate program `NAME`, or a duplicate or non-literal reactor `NAMESPACE`;
/// - a program whose `MODE` is not `Mode::Pure`;
/// - a `boot` or `default` that names a program or reactor.
#[macro_export]
macro_rules! bundle {
    (@aether_export_generate
        { remaining_generators: [$($rest:path),*] }
        {
            boot: $boot:tt,
            default: $default:tt,
            actors: [$($actors:tt)*],
            exports: [$($exports:tt)*],
            private: [$($private:tt)*]
        }
    ) => {
        $crate::__bundle_export_generate! {
            remaining_generators: [$($rest),*]
            boot: $boot
            default: $default
            actors: [$($actors)*]
            exports: [$($exports)*]
            private: [$($private)*]
        }
    };
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "bundle is an export! generator; write `export!(..., generators = [aether_bloomery_bundle::bundle])`"
        );
    };
}
