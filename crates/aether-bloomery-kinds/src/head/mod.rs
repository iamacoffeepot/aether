//! Named heads: a typed symbol and the one stored move event.

mod codec;
mod symbol;

use core::fmt;
use core::hash::{Hash, Hasher};

use crate::{Digest, Ref};

pub use symbol::{RecordedSymbol, Symbol, SymbolError};

/// Points the named head at a typed target. Last move wins.
///
/// Produced by [`Symbol::move_to`]. Accessors return the symbol and target
/// infallibly after a successful decode. Destination existence is checked
/// by the journal at append, not here.
pub struct HeadMoved<K> {
    symbol: Symbol<K>,
    to: Ref<K>,
}

/// Runtime form of [`HeadMoved`]: a recorded symbol plus a destination digest.
///
/// Journal validation uses [`RecordedSymbol::kind`] as the expected prefix.
/// There is no registry and no target-type dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHeadMove {
    symbol: RecordedSymbol,
    to: Digest,
}

impl<K> HeadMoved<K> {
    pub(crate) fn from_parts(symbol: Symbol<K>, to: Ref<K>) -> Self {
        Self { symbol, to }
    }

    /// Borrow the typed symbol this event moves.
    #[must_use]
    pub const fn symbol(&self) -> &Symbol<K> {
        &self.symbol
    }

    /// The typed destination. Existence is not proven here.
    #[must_use]
    pub const fn to(&self) -> Ref<K> {
        self.to
    }
}

impl RecordedHeadMove {
    /// Bind a recorded identity to a destination digest.
    #[must_use]
    pub fn new(symbol: RecordedSymbol, to: Digest) -> Self {
        Self { symbol, to }
    }

    /// Borrow the recorded identity `(kind, name)`.
    #[must_use]
    pub const fn symbol(&self) -> &RecordedSymbol {
        &self.symbol
    }

    /// Destination digest. The expected prefix is [`RecordedSymbol::kind`].
    #[must_use]
    pub const fn to(&self) -> Digest {
        self.to
    }
}

impl<K: aether_data::Kind> From<&HeadMoved<K>> for RecordedHeadMove {
    fn from(event: &HeadMoved<K>) -> Self {
        Self { symbol: RecordedSymbol::from(event.symbol()), to: event.to().digest() }
    }
}

impl<K> Clone for HeadMoved<K> {
    fn clone(&self) -> Self {
        Self { symbol: self.symbol.clone(), to: self.to }
    }
}

impl<K> PartialEq for HeadMoved<K> {
    fn eq(&self, other: &Self) -> bool {
        self.symbol == other.symbol && self.to == other.to
    }
}

impl<K> Eq for HeadMoved<K> {}

impl<K> Hash for HeadMoved<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.symbol.hash(state);
        self.to.hash(state);
    }
}

impl<K> fmt::Debug for HeadMoved<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeadMoved").field("symbol", &self.symbol).field("to", &self.to).finish()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::{Citations, Cites, Kind};

    use crate::{Digest, Program, Ref, Tree};

    use super::{RecordedHeadMove, RecordedSymbol, Symbol};

    #[test]
    fn independent_heads_may_share_a_symbol() {
        // Catches a Symbol that mixed the name into a kind-level unique
        // identity, so a program head and a tree head could not share a name,
        // or a move that wrote the wrong KindId.
        const PROGRAM: Symbol<Program> = Symbol::new("main");
        const TREE: Symbol<Tree> = Symbol::new("main");
        let program_ref = Ref::<Program>::from_digest(Digest::from_bytes([1; 32]));
        let tree_ref = Ref::<Tree>::from_digest(Digest::from_bytes([2; 32]));
        let program_event = PROGRAM.move_to(program_ref);
        let tree_event = TREE.move_to(tree_ref);
        assert_eq!(program_event.symbol().as_str(), "main");
        assert_eq!(tree_event.symbol().as_str(), "main");
        assert_eq!(program_event.symbol().kind(), Program::ID);
        assert_eq!(tree_event.symbol().kind(), Tree::ID);
        assert_ne!(program_event.symbol().kind(), tree_event.symbol().kind());
        assert_eq!(program_event.to(), program_ref);
        assert_eq!(tree_event.to(), tree_ref);
        let recorded = RecordedHeadMove::from(&program_event);
        assert_eq!(recorded.symbol().kind(), Program::ID);
        assert_eq!(recorded.to(), program_ref.digest());
    }

    #[test]
    fn symbols_and_moves_cite_nothing() {
        // Catches a Cites walk that treated a symbol or head-move destination
        // as an ordinary Ref, so Draft would skip the recognized-event check
        // or a symbol would cite its current target.
        const MAIN: Symbol<Tree> = Symbol::new("main");
        let tree = Ref::<Tree>::from_digest(Digest::from_bytes([3; 32]));
        let event = MAIN.move_to(tree);
        let recorded_symbol = RecordedSymbol::from(&MAIN);
        let recorded_event = RecordedHeadMove::from(&event);

        let mut sink = Citations::default();
        MAIN.cites(&mut sink);
        event.cites(&mut sink);
        recorded_symbol.cites(&mut sink);
        recorded_event.cites(&mut sink);
        assert!(sink.into_vec().is_empty());
    }

    #[test]
    fn clone_eq_and_debug_do_not_need_traits_on_k() {
        // Catches a derive that bounded Clone/Eq/Debug on K, so a head of a
        // non-Clone kind could not be cloned or compared.
        struct Marker;
        impl Kind for Marker {
            const NAME: &'static str = "test.bloomery.head.marker";
            const ID: aether_data::KindId = aether_data::storage_kind_id_from_name(Self::NAME);
        }
        let symbol = Symbol::<Marker>::new("main");
        let cloned = symbol.clone();
        assert_eq!(symbol, cloned);
        assert_eq!(alloc::format!("{symbol:?}"), r#"Symbol("main")"#);
        let event = symbol.move_to(Ref::from_digest(Digest::from_bytes([4; 32])));
        assert_eq!(event, event.clone());
        let _ = alloc::format!("{event:?}");
    }
}
