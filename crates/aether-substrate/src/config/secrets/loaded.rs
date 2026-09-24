//! [`Secrets`]: the values one consumer's [`SecretRefs`](super::SecretRefs)
//! loaded, each still paired with the key and name that bound it.

use std::vec;

use super::name::SecretName;
use super::secret::Secret;

/// The loaded collection of `(key, SecretName, Secret)` entries, in binding
/// order. Dropping it wipes every value it still holds. A consumer iterates it
/// by value, moving each [`Secret`] into its own table.
#[derive(Debug, Default)]
pub struct Secrets {
    entries: Vec<(String, SecretName, Secret)>,
}

impl Secrets {
    /// Assemble a collection directly. [`SecretRefs::load`](super::SecretRefs::load)
    /// builds one from the secrets directory; a consumer crate's tests build
    /// one from obviously fake values.
    #[must_use]
    pub fn from_entries(entries: Vec<(String, SecretName, Secret)>) -> Self {
        Self { entries }
    }
}

impl IntoIterator for Secrets {
    type Item = (String, SecretName, Secret);
    type IntoIter = vec::IntoIter<(String, SecretName, Secret)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}
