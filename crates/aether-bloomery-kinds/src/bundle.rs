//! Namespace and section names of the one generated bundle root.

/// Declared namespace of the one generated bundle root, which the export selector names: the driver loads every bundle
/// with `export: Some(BUNDLE_NAMESPACE)`. Every bundle is content-addressed, so the root publishes as
/// `aether.bloomery.bundle.<module hash>` (ADR-0241 §3) and every built bundle is its own publication.
pub const BUNDLE_NAMESPACE: &str = "aether.bloomery.bundle";
/// Custom-section name of a bundle's program declarations.
pub const PROGRAMS_SECTION: &str = "aether.bloomery.programs";
