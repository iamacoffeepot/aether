//! Session-scoped id allocation, shared by every capability that mints
//! ids it hands back to a caller.
//!
//! The capabilities each grew their own monotonic counter and disagreed
//! about the ceiling: some incremented unchecked (a debug panic, a
//! release wrap), some saturated and then handed the ceiling id out
//! forever, quietly aliasing the entry a caller already held. None of
//! them could say "no". [`SessionIds`] has one behaviour: it walks its
//! window once, then reports exhaustion so the caller replies `Err`
//! rather than issuing a live id a second time.
//!
//! The window is inclusive and may reserve ids at either end — the
//! audio cap's sampled banks start above the compiled-in built-ins, and
//! the render cap's textures stop below its reserved internal id.

/// An id space [`SessionIds`] can walk: where a fresh allocator starts,
/// how far the representation reaches, and the step between ids.
pub trait SessionId: Copy + Ord {
    /// The id a fresh allocator hands out first.
    const FIRST: Self;

    /// The highest id the space can represent — the ceiling of an
    /// allocator that reserves nothing at the top.
    const LAST: Self;

    /// The id following `self`, or `None` at [`Self::LAST`].
    fn advance(self) -> Option<Self>;
}

macro_rules! impl_session_id {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl SessionId for $ty {
                const FIRST: Self = 0;
                const LAST: Self = Self::MAX;

                fn advance(self) -> Option<Self> {
                    self.checked_add(1)
                }
            }
        )+
    };
}

impl_session_id!(u8, u32, u64);

/// A monotonic source of session-scoped ids over an inclusive window.
///
/// Ids depend only on allocation order, so they are stable for the life
/// of the session and are never recycled: a rejected request that never
/// calls [`Self::allocate`] leaves the sequence dense over accepted
/// ones, and a window that runs out stays out.
#[derive(Debug)]
pub struct SessionIds<T> {
    /// The id the next [`SessionIds::allocate`] hands out, or `None`
    /// once the window is spent. Exhaustion is terminal — an id already
    /// handed out is never offered again, so a live entry cannot be
    /// overwritten by a later allocation.
    next: Option<T>,
    /// The last id the window includes.
    last: T,
}

impl<T: SessionId> SessionIds<T> {
    /// An allocator over the whole space, `T::FIRST ..= T::LAST`.
    #[must_use]
    pub fn new() -> Self {
        Self::range(T::FIRST, T::LAST)
    }

    /// An allocator over the inclusive `first ..= last` window, for a
    /// space with ids reserved at either end. An empty window (`first`
    /// above `last`) starts exhausted.
    #[must_use]
    pub fn range(first: T, last: T) -> Self {
        Self { next: (first <= last).then_some(first), last }
    }

    /// The id the next successful [`Self::allocate`] will hand out, or
    /// `None` once the window is spent. Lets a caller that validates
    /// before allocating show it left the sequence untouched.
    #[must_use]
    pub fn peek(&self) -> Option<T> {
        self.next
    }

    /// Take the next id, or `None` when the window is spent.
    pub fn allocate(&mut self) -> Option<T> {
        let id = self.next?;
        self.next = if id < self.last {
            id.advance()
        } else {
            None
        };
        Some(id)
    }
}

impl<T: SessionId> Default for SessionIds<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::SessionIds;

    /// Tripwire: the window's last id is handed out exactly once and
    /// exhaustion is terminal. The counters this replaced either wrapped
    /// past the ceiling or saturated onto it, re-issuing a live id to
    /// every later caller.
    #[test]
    fn the_last_id_is_issued_once_and_exhaustion_is_terminal() {
        let mut ids = SessionIds::range(u8::MAX - 1, u8::MAX);
        assert_eq!(ids.allocate(), Some(u8::MAX - 1));
        assert_eq!(ids.allocate(), Some(u8::MAX));
        assert_eq!(ids.allocate(), None, "the ceiling id must not be issued twice");
        assert_eq!(ids.allocate(), None, "exhaustion is terminal");
        assert_eq!(ids.peek(), None);
    }

    /// Tripwire: a reserved tail is unreachable. The render cap keeps
    /// `u32::MAX` for its internal white texture, so an allocator that
    /// walked to the representation ceiling would eventually alias it.
    #[test]
    fn a_reserved_tail_is_never_allocated() {
        let mut ids = SessionIds::range(0_u32, u32::MAX - 1);
        assert_eq!(ids.allocate(), Some(0));

        let mut spent = SessionIds::range(u32::MAX - 1, u32::MAX - 1);
        assert_eq!(spent.allocate(), Some(u32::MAX - 1));
        assert_eq!(spent.allocate(), None, "the reserved id is outside the window");
    }
}
