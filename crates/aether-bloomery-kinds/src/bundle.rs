//! Namespace and section names of the one generated bundle root.

/// Export namespace of the one generated bundle root. The driver loads every bundle with `export: Some(BUNDLE_NAMESPACE)`.
pub const BUNDLE_NAMESPACE: &str = "aether.bloomery.bundle";
/// Custom-section name of a bundle's program declarations.
pub const PROGRAMS_SECTION: &str = "aether.bloomery.programs";
