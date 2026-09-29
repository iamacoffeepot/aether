//! Declared-dependency liveness read (ADR-0230): the namespace of the first
//! `depends(R)` entry with no `Live` route, for both transports.
//!
//! The component host's check moved here so native births reuse it: one
//! derivation of a dependency's position and one liveness read, whether the
//! declaration arrived as a wasm `InputsRecord::Dependency` or a native
//! actor's `Declared::Depends` list.

use aether_actor::{DependencyResolver, One, Resolve};

use super::Registry;

impl Registry {
    /// The namespace of the first declared dependency with no `Live` route, or
    /// `None` when every entry is live. Every declarable dependency is a root
    /// singleton (ADR-0241 §5): its [`One`] entry folds to the root position
    /// whatever the placement, and the registry answers whether a `Live`
    /// route is there. A missing dependency is a refusal and nothing else: no
    /// ordering, no retry, no wait.
    pub(crate) fn missing_dependency<'a>(
        &self,
        dependencies: impl IntoIterator<Item = (u8, &'a str)>,
    ) -> Option<&'a str> {
        dependencies.into_iter().find_map(|(resolver, namespace)| {
            // The inputs reader rejects a tag no strategy claims, so an
            // unknown tag never arrives here; refuse closed anyway.
            let candidate = (resolver == One::TAG).then(|| One::candidate(0, namespace, None)).flatten();
            match candidate {
                Some(id) if self.is_live_at(id) => None,
                _ => Some(namespace),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::mail::registry::noop_handler;
    use crate::testing::boot_authority;

    use super::*;

    #[test]
    fn dependency_folds_match_registered_positions() {
        let registry = Registry::new();
        registry.register_inbox(&boot_authority(), "test.dependency.one", noop_handler());

        // A `One` dependency folds from the root, so the registered root
        // name satisfies it.
        assert_eq!(registry.missing_dependency([(One::TAG, "test.dependency.one")]), None);

        // An absent namespace is missing, and an unknown tag refuses closed
        // even when its namespace is live.
        assert_eq!(registry.missing_dependency([(One::TAG, "test.dependency.absent")]), Some("test.dependency.absent"));
        assert_eq!(registry.missing_dependency([(0xFF, "test.dependency.one")]), Some("test.dependency.one"));

        let none: [(u8, &str); 0] = [];
        assert_eq!(registry.missing_dependency(none), None);
    }
}
