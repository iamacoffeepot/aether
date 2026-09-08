//! Which model-process instruction bundles this host authorizes (ADR-0214).

use std::collections::BTreeSet;

use aether_bloomery::{Digest, decode_hex};

/// The instruction-bundle addresses the host operator authorized to serve as
/// model-process policy (ADR-0214 §Model-process instructions are explicit
/// configuration), as resolved from the coordinator's own configuration at boot.
///
/// Authorization is deliberately not derivable from anything a bloom, a
/// candidate, or a request carries: uploading a bundle, knowing its digest, or
/// naming it in a sealed registry does not authorize it. This value is where the
/// operator's answer enters the process, and the boot path writes it to the
/// store's `authorized_instructions` table, which is what the dispatch gate
/// reads. Nothing the material under examination can write reaches either.
///
/// The default is empty, and an empty set authorizes nothing. That is the
/// fail-closed arm ADR-0149 asks for: a host that has stated no policy has no
/// policy, and every model dispatch under it refuses rather than falling back to
/// whatever instructions the checkout happens to carry.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProcessPolicy {
    authorized: BTreeSet<Digest>,
}

impl ProcessPolicy {
    /// Read the coordinator's comma-separated authorized-bundle knob.
    ///
    /// An entry that is not a 32-byte lowercase-hex digest is dropped with an
    /// error log rather than guessed at. Dropping is the fail-closed arm: an
    /// operator who mistyped a digest gets refusals naming the bundle that was
    /// actually pinned, plus this line naming what could not be read.
    #[must_use]
    pub fn parse(configured: &str) -> Self {
        configured
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| {
                let parsed = decode_hex(entry).as_deref().and_then(Digest::from_slice);
                if parsed.is_none() {
                    tracing::error!(
                        target: "aether_chassis_bloomery::provenance",
                        entry,
                        "authorized instruction bundle is not a 32-byte hex digest; dropping it from the policy",
                    );
                }
                parsed
            })
            .collect()
    }

    /// The authorized addresses as the raw digests the store table is keyed by.
    #[must_use]
    pub fn addresses(&self) -> Vec<Vec<u8>> {
        self.authorized.iter().map(|digest| digest.as_bytes().to_vec()).collect()
    }

    /// Whether the host stated no policy at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.authorized.is_empty()
    }

    /// How many bundles the host authorized.
    #[must_use]
    pub fn len(&self) -> usize {
        self.authorized.len()
    }
}

impl FromIterator<Digest> for ProcessPolicy {
    fn from_iter<I: IntoIterator<Item = Digest>>(iter: I) -> Self {
        Self { authorized: iter.into_iter().collect() }
    }
}
