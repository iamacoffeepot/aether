//! `ReactorBundle` export generator: a macro hook over host-side proc-macro codegen.
//!
//! `export!` collects framework-owned descriptor envelopes into `actors` and the
//! listed types into `exports`, then invokes
//! `$gen!(@aether_export_generate { remaining_generators } { boot, default, actors, exports })`.
//! This macro is that hook. It forwards into `__reactor_export_generate`, which
//! selects the `aether_bloomery_reactor` extension on exported paths, emits the
//! hidden coordinator and inline peers, rewrites `exports` only, and appends a
//! coordinator envelope to `actors`. Original reactor envelopes stay attached to
//! their types. Macros cannot reflect on target-crate trait impls, so there is
//! no empty host-side generator trait. The derive crate is `proc-macro = true`
//! and is not linked into guest wasm.

/// Framework-owned mailbox namespace of the generated views coordinator.
///
/// One coordinator is emitted per `export!(…, generators = [ReactorBundle])`.
/// Load it with this selector. Reactor peers use each reactor's own
/// `NAMESPACE` and are inline children, not module exports. Multiple loaded
/// cluster instances remain isolated. The coordinator is never a `boot` actor.
pub const CLUSTER_NAMESPACE: &str = "aether.bloomery.reactor";

/// Export generator for bloomery reactors.
///
/// Exact author form:
///
/// ```ignore
/// aether_actor::export!(
///     default = Probe,
///     ProbeWithConfig,
///     SourcePublisher,
///     SourceWitness,
///     ReactorOutputSink,
///     generators = [aether_bloomery_reactor::ReactorBundle],
/// );
/// ```
///
/// The generator reads `actors` envelopes for the current `exports` paths,
/// keeps ordinary actors in the export selection, and replaces selected
/// reactors with one hidden views coordinator. Reactor types remain in
/// `actors` with their own extensions; those extensions are not copied onto
/// the coordinator. Zero reactors, a `default` that is a reactor,
/// reserved/duplicate `NAMESPACE`, and missing companions are compile errors
/// — entries are never dropped silently. `reactor` already names the
/// attribute macro, so this generator is not `reactor!`.
#[macro_export]
macro_rules! ReactorBundle {
    (@aether_export_generate
        { remaining_generators: [$($rest:path),*] }
        { boot: $boot:tt, default: $default:tt, actors: [$($actors:tt)*], exports: [$($exports:tt)*] }
    ) => {
        $crate::__reactor_export_generate! {
            remaining_generators: [$($rest),*]
            boot: $boot
            default: $default
            actors: [$($actors)*]
            exports: [$($exports)*]
        }
    };
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "ReactorBundle is an export! generator; write `export!(..., generators = [ReactorBundle])`"
        );
    };
}
