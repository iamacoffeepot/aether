//! [`Namespace`](crate::Namespace): a compile-time-validated actor type name.

use super::load_name::LoadName;
use super::segment::check_segment;
use crate::ActorId;

/// An actor type name whose grammar was checked when it was written. The
/// text is consumed only by folding it into an [`ActorId`]: there is no
/// `as_str`, `Deref`, `Display`, or `AsRef<str>`, so a `Namespace` can be
/// compared and logged but never formatted into a path or hashed directly.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Namespace(&'static str);

impl Namespace {
    /// Validate `text` as a namespace segment: non-empty, at most 256
    /// bytes, no `:` or `/`, no control or whitespace characters.
    ///
    /// # Panics
    ///
    /// Panics with the violated rule's message when `text` breaks the
    /// grammar. In a `const` context the panic is a compile error, so a
    /// bad `NAMESPACE` literal never reaches registration:
    ///
    /// ```compile_fail,E0080
    /// use aether_data::Namespace;
    ///
    /// const BAD: Namespace = Namespace::new("a/b");
    /// ```
    #[must_use]
    pub const fn new(text: &'static str) -> Self {
        if let Err(fault) = check_segment(text.as_bytes()) {
            panic!("{}", fault.message());
        }
        Self(text)
    }

    /// This namespace as a singleton [`ActorId`], `hash(NAMESPACE)`.
    #[must_use]
    pub const fn actor_id(self) -> ActorId {
        ActorId::singleton(self.0)
    }

    /// This namespace plus `key` as an instanced [`ActorId`],
    /// `hash(NAMESPACE:subname)`.
    #[must_use]
    pub const fn instanced_actor_id(self, key: &LoadName) -> ActorId {
        ActorId::instanced(self.0, key.as_str())
    }
}
