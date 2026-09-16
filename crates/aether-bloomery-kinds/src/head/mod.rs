//! Named heads: a typed identity and the one stored move event.

mod symbol;

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::{Kind, KindId};

use crate::{Digest, Ref};

pub use symbol::{Symbol, SymbolError};

/// Typed identity of a mutable slot. The type parameter is a phantom
/// marker, as on [`Ref`]; this type is not stored, does not cite, and
/// does not capture a current binding.
pub struct Head<K> {
    symbol: Symbol,
    _kind: PhantomData<fn() -> K>,
}

/// Intent to point a `Head<K>` at a [`Ref`]. Not an executed
/// operation. The only construction path is [`Head::move_to`].
pub struct Move<K> {
    head: Head<K>,
    to: Ref<K>,
}

/// Points the named head `(target_kind, symbol)` at `to`. Last move wins.
/// First binding, later replacement, a move to the current target, and
/// pointing back at an older artifact are all this one event.
///
/// Intrinsic field values are valid at construction and decode. Destination
/// existence is checked by the journal at append, not here.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.head_moved")]
pub struct HeadMoved {
    /// Kind of the immutable content this head names.
    pub target_kind: KindId,
    /// Untyped name of the slot. Unique per `target_kind`.
    pub symbol: Symbol,
    /// An artifact whose kind is `target_kind`. Untyped because that kind is
    /// known only at runtime.
    pub to: Digest,
}

impl<K> Head<K> {
    /// Wrap an already-valid symbol. Infallible because the symbol is already valid.
    #[must_use]
    pub fn new(symbol: Symbol) -> Self {
        Self { symbol, _kind: PhantomData }
    }

    /// Borrow the symbol that identifies this slot.
    #[must_use]
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Point this head at `to`. The result is intent, not an executed move.
    ///
    /// A mismatched target kind is rejected at compile time:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Digest, Head, Program, Ref, Symbol, Tree};
    ///
    /// let head = Head::<Tree>::new(Symbol::new("main").unwrap());
    /// let program = Ref::<Program>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = head.move_to(program);
    /// ```
    ///
    /// The same-kind call compiles:
    ///
    /// ```
    /// use aether_bloomery_kinds::{Digest, Head, Ref, Symbol, Tree};
    ///
    /// let head = Head::<Tree>::new(Symbol::new("main").unwrap());
    /// let tree = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _intent = head.move_to(tree);
    /// ```
    #[must_use]
    pub fn move_to(&self, to: Ref<K>) -> Move<K> {
        Move { head: self.clone(), to }
    }
}

impl<K: Kind> Move<K> {
    /// Erase the compile-time target type into [`HeadMoved`].
    ///
    /// `target_kind` is `K::ID`. The journal still rechecks that `to`
    /// exists under that kind; a typed [`Ref`] does not prove it.
    #[must_use]
    pub fn into_event(self) -> HeadMoved {
        HeadMoved { target_kind: K::ID, symbol: self.head.symbol, to: self.to.digest() }
    }
}

impl<K> Clone for Head<K> {
    fn clone(&self) -> Self {
        Self { symbol: self.symbol.clone(), _kind: PhantomData }
    }
}

impl<K> PartialEq for Head<K> {
    fn eq(&self, other: &Self) -> bool {
        self.symbol == other.symbol
    }
}

impl<K> Eq for Head<K> {}

impl<K> Hash for Head<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.symbol.hash(state);
    }
}

impl<K> fmt::Debug for Head<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Head").field(&self.symbol).finish()
    }
}

impl<K> Clone for Move<K> {
    fn clone(&self) -> Self {
        Self { head: self.head.clone(), to: self.to }
    }
}

impl<K> PartialEq for Move<K> {
    fn eq(&self, other: &Self) -> bool {
        self.head == other.head && self.to == other.to
    }
}

impl<K> Eq for Move<K> {}

impl<K> fmt::Debug for Move<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Move").field("head", &self.head).field("to", &self.to).finish()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;

    use crate::{Digest, Program, Ref, Tree};

    use super::{Head, Symbol};

    #[test]
    fn independent_heads_may_share_a_symbol() {
        // Catches a Head that mixed the symbol into a kind-level unique
        // identity, so a program head and a tree head could not share a name,
        // or an into_event that wrote the wrong KindId.
        let symbol = Symbol::new("main").expect("valid symbol");
        let program = Head::<Program>::new(symbol.clone());
        let tree = Head::<Tree>::new(symbol);
        let program_ref = Ref::<Program>::from_digest(Digest::from_bytes([1; 32]));
        let tree_ref = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));
        let program_event = program.move_to(program_ref).into_event();
        let tree_event = tree.move_to(tree_ref).into_event();
        assert_eq!(program_event.symbol.as_str(), "main");
        assert_eq!(tree_event.symbol.as_str(), "main");
        assert_eq!(program_event.target_kind, Program::ID);
        assert_eq!(tree_event.target_kind, Tree::ID);
        assert_ne!(program_event.target_kind, tree_event.target_kind);
        assert_eq!(program_event.to, Digest::from_bytes([1; 32]));
        assert_eq!(tree_event.to, Digest::from_bytes([2; 32]));
    }
}
