//! Declared-dependency liveness read (ADR-0230): the namespace of the first
//! `depends(R)` entry with no `Live` route, for both transports.
//!
//! The component host's check moved here so native births reuse it: one
//! derivation of a dependency's position and one liveness read, whether the
//! declaration arrived as a wasm `InputsRecord::Dependency` or a native
//! link-time `DependencyEntry`.

use aether_actor::{DependencyResolver, Embedded, One, Resolve};

use crate::mail::MailboxId;

use super::Registry;

impl Registry {
    /// The namespace of the first declared dependency with no `Live` route, or
    /// `None` when every entry is live. Each entry folds to its position
    /// through its own strategy — [`One`] at the root, [`Embedded`] beneath
    /// the placement's `parent` — and the registry answers whether a `Live`
    /// route is there. A missing dependency is a refusal and nothing else: no
    /// ordering, no retry, no wait.
    pub fn missing_dependency<'a>(
        &self,
        parent: MailboxId,
        dependencies: impl IntoIterator<Item = (u8, &'a str)>,
    ) -> Option<&'a str> {
        dependencies.into_iter().find_map(|(resolver, namespace)| {
            let candidate = match resolver {
                One::TAG => One::candidate(MailboxId::NONE.0, namespace, None),
                Embedded::TAG => Embedded::candidate(parent.0, namespace, None),
                // The inputs reader rejects a tag no strategy claims, so an
                // unknown tag never arrives here; refuse closed anyway.
                _ => return Some(namespace),
            };
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
        let authority = boot_authority();
        registry.register_inbox(&authority, "test.dependency.one", noop_handler());
        let parent = registry.register_inbox(&authority, "test.dependency.parent", noop_handler());
        let elsewhere = registry.register_inbox(&authority, "test.dependency.elsewhere", noop_handler());
        // The spawn path registers nested actors under a caller-folded id;
        // the test folds the peer's the same way `missing_dependency` does.
        // (The harness scenario pins the fold against the real spawn path.)
        let Some(peer_id) = Embedded::candidate(parent.0, "test.dependency.peer", None) else {
            panic!("keyless candidate is Some");
        };
        assert!(
            registry
                .try_register_inbox_with_id(
                    &authority,
                    peer_id,
                    "test.dependency.parent/aether.embedded:test.dependency.peer",
                    noop_handler(),
                )
                .is_ok()
        );

        // A `One` dependency folds from the root, so the registered root
        // name satisfies it under any placement parent, including `NONE`.
        let one = (One::TAG, "test.dependency.one");
        assert_eq!(registry.missing_dependency(parent, [one]), None);
        assert_eq!(registry.missing_dependency(MailboxId::NONE, [one]), None);

        // An `Embedded` dependency folds beneath the placement's parent —
        // and only there.
        let peer = (Embedded::TAG, "test.dependency.peer");
        assert_eq!(registry.missing_dependency(parent, [peer]), None);
        assert_eq!(registry.missing_dependency(elsewhere, [peer]), Some("test.dependency.peer"));

        // An absent namespace is missing, and an unknown tag refuses closed
        // even when its namespace is live.
        let absent = (One::TAG, "test.dependency.absent");
        assert_eq!(registry.missing_dependency(parent, [absent]), Some("test.dependency.absent"));
        let unknown = (0xFF, "test.dependency.one");
        assert_eq!(registry.missing_dependency(parent, [unknown]), Some("test.dependency.one"));

        let none: [(u8, &str); 0] = [];
        assert_eq!(registry.missing_dependency(parent, none), None);
    }
}
