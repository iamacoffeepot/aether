//! Typed and recorded head names: checked const heads and owned runtime identities.

use alloc::borrow::Cow;
use alloc::string::String;
use core::cmp::Ordering;
use core::error::Error as StdError;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::{Kind, KindId, StorageError};

use crate::Ref;

use super::HeadMoved;

const MAX_BYTES: usize = 128;
const KIND_MISMATCH: &str = "kind-mismatch";
const INVARIANT_KIND: &str = "Head";

/// Why a head name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadNameError {
    /// The string was empty.
    Empty,
    /// The string was longer than [`Head::MAX_BYTES`].
    TooLong,
    /// The string contained a character for which [`char::is_whitespace`] is true.
    Whitespace,
    /// The string contained a character for which [`char::is_control`] is true.
    Control,
}

impl HeadNameError {
    pub(super) const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Whitespace => "whitespace",
            Self::Control => "control",
        }
    }
}

impl aether_data::Invariant for HeadNameError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for HeadNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for HeadNameError {}

/// Typed head name `(K::ID, name)`. Declare it as a `const` or `static`.
///
/// The name is 1–128 UTF-8 bytes with no whitespace or control. Bytes are
/// preserved exactly. Equality, hashing, and ordering are case-sensitive and
/// normalization-free. Punctuation has no special semantics. A head is not
/// a [`crate::ProgramName`], not a citation, and not a captured current value.
///
/// ```
/// use aether_bloomery_kinds::{Head, Program, Tree};
///
/// const MAIN: Head<Tree> = Head::new("main");
/// static BUILD: Head<Program> = Head::new("build");
/// assert_eq!(MAIN.as_str(), "main");
/// assert_eq!(BUILD.as_str(), "build");
/// ```
///
/// ```compile_fail
/// use aether_bloomery_kinds::{Head, Tree};
/// const _: Head<Tree> = Head::new("");
/// ```
///
/// ```compile_fail
/// use aether_bloomery_kinds::{Head, Tree};
/// const _: Head<Tree> = Head::new(" ");
/// ```
///
/// ```compile_fail
/// use aether_bloomery_kinds::{Head, Tree};
/// const _: Head<Tree> = Head::new("\n");
/// ```
///
/// ```compile_fail
/// use aether_bloomery_kinds::{Head, Tree};
/// const _: Head<Tree> = Head::new(
///     "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
/// );
/// ```
pub struct Head<K> {
    name: Cow<'static, str>,
    _kind: PhantomData<fn() -> K>,
}

/// Runtime head identity: a kind plus a validated name.
///
/// Journal and index decoding use this form. Construction checks the name
/// with the same rules as [`Head::new`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordedHead {
    kind: KindId,
    name: String,
}

impl<K> Head<K> {
    /// Maximum accepted UTF-8 byte length, inclusive.
    pub const MAX_BYTES: usize = MAX_BYTES;

    /// Borrow the head name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.name.as_ref()
    }
}

impl<K: Kind> Head<K> {
    /// Accept a `'static` name, borrowing the literal.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty, longer than [`Self::MAX_BYTES`], or contains
    /// a character for which [`char::is_whitespace`] or [`char::is_control`] is
    /// true. Length is checked before characters. A character that is both
    /// control and whitespace is control. In a `const` or `static` initializer
    /// this fails compilation.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        if let Err(error) = check(name) {
            match error {
                HeadNameError::Empty => panic!("empty"),
                HeadNameError::TooLong => panic!("too-long"),
                HeadNameError::Whitespace => panic!("whitespace"),
                HeadNameError::Control => panic!("control"),
            }
        }
        Self { name: Cow::Borrowed(name), _kind: PhantomData }
    }

    /// Kind this head names, `K::ID`.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        K::ID
    }

    /// Point this head at `to`. The result is the typed move event.
    ///
    /// A mismatched target kind is rejected at compile time:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Digest, Head, Program, Ref, Tree};
    ///
    /// const MAIN: Head<Tree> = Head::new("main");
    /// let program = Ref::<Program>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = MAIN.move_to(program);
    /// ```
    ///
    /// The same-kind call compiles:
    ///
    /// ```
    /// use aether_bloomery_kinds::{Digest, Head, Ref, Tree};
    ///
    /// const MAIN: Head<Tree> = Head::new("main");
    /// let tree = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _event = MAIN.move_to(tree);
    /// ```
    #[must_use]
    pub fn move_to(&self, to: Ref<K>) -> HeadMoved<K> {
        HeadMoved::from_parts(self.clone(), to)
    }

    pub(super) fn from_recorded(recorded: RecordedHead) -> Result<Self, StorageError> {
        if recorded.kind != K::ID {
            return Err(head_kind_mismatch());
        }
        Ok(Self { name: Cow::Owned(recorded.name), _kind: PhantomData })
    }
}

impl RecordedHead {
    /// Maximum accepted UTF-8 byte length, inclusive.
    pub const MAX_BYTES: usize = MAX_BYTES;

    /// Accept a runtime kind plus a name of 1–128 UTF-8 bytes with no
    /// whitespace or control.
    ///
    /// # Errors
    ///
    /// [`HeadNameError`] names which rule failed. Length is checked before
    /// characters. A character that is both control and whitespace is
    /// [`HeadNameError::Control`].
    pub fn new(kind: KindId, name: impl Into<String>) -> Result<Self, HeadNameError> {
        let name = name.into();
        check(&name)?;
        Ok(Self { kind, name })
    }

    /// Kind stored with this identity.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Borrow the name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl<K: Kind> From<&Head<K>> for RecordedHead {
    fn from(head: &Head<K>) -> Self {
        Self { kind: K::ID, name: String::from(head.as_str()) }
    }
}

impl<K> Clone for Head<K> {
    fn clone(&self) -> Self {
        Self { name: self.name.clone(), _kind: PhantomData }
    }
}

impl<K> PartialEq for Head<K> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<K> Eq for Head<K> {}

impl<K> PartialOrd for Head<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Head<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl<K> Hash for Head<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl<K> fmt::Debug for Head<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Head").field(&self.as_str()).finish()
    }
}

pub(super) const fn check(value: &str) -> Result<(), HeadNameError> {
    if value.is_empty() {
        return Err(HeadNameError::Empty);
    }
    if value.len() > MAX_BYTES {
        return Err(HeadNameError::TooLong);
    }
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let (ch, next) = next_scalar(bytes, i);
        if ch.is_control() {
            return Err(HeadNameError::Control);
        }
        if ch.is_whitespace() {
            return Err(HeadNameError::Whitespace);
        }
        i = next;
    }
    Ok(())
}

pub(super) fn head_storage_err(error: HeadNameError) -> StorageError {
    StorageError::Invariant { kind: INVARIANT_KIND, reason: error.reason() }
}

pub(super) fn head_kind_mismatch() -> StorageError {
    StorageError::Invariant { kind: INVARIANT_KIND, reason: KIND_MISMATCH }
}

/// Decode one Unicode scalar from already-valid UTF-8 `str` bytes.
const fn next_scalar(bytes: &[u8], i: usize) -> (char, usize) {
    let b0 = bytes[i];
    if b0 < 0x80 {
        return (b0 as char, i + 1);
    }
    if b0 < 0xE0 {
        let code = ((b0 & 0x1F) as u32) << 6 | (bytes[i + 1] & 0x3F) as u32;
        return (unwrap_scalar(code), i + 2);
    }
    if b0 < 0xF0 {
        let code = ((b0 & 0x0F) as u32) << 12 | ((bytes[i + 1] & 0x3F) as u32) << 6 | (bytes[i + 2] & 0x3F) as u32;
        return (unwrap_scalar(code), i + 3);
    }
    let code = ((b0 & 0x07) as u32) << 18
        | ((bytes[i + 1] & 0x3F) as u32) << 12
        | ((bytes[i + 2] & 0x3F) as u32) << 6
        | (bytes[i + 3] & 0x3F) as u32;
    (unwrap_scalar(code), i + 4)
}

const fn unwrap_scalar(code: u32) -> char {
    match char::from_u32(code) {
        Some(ch) => ch,
        None => panic!("str bytes are valid UTF-8"),
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell;
    use core::marker::PhantomData;

    use aether_data::{Kind, KindId};

    use crate::{Program, Tree};

    use super::{Head, HeadNameError, RecordedHead, check, next_scalar};

    fn check_by_chars(value: &str) -> Result<(), HeadNameError> {
        if value.is_empty() {
            return Err(HeadNameError::Empty);
        }
        if value.len() > Head::<Tree>::MAX_BYTES {
            return Err(HeadNameError::TooLong);
        }
        for ch in value.chars() {
            if ch.is_control() {
                return Err(HeadNameError::Control);
            }
            if ch.is_whitespace() {
                return Err(HeadNameError::Whitespace);
            }
        }
        Ok(())
    }

    #[test]
    fn const_and_static_declarations_borrow_the_literal() {
        // Catches a constructor that forced an owned String, so a const/static
        // declaration could not compile, or that stored a different spelling.
        const MAIN: Head<Tree> = Head::new("main");
        static BUILD: Head<Program> = Head::new("build");
        assert_eq!(MAIN.as_str(), "main");
        assert_eq!(MAIN.kind(), Tree::ID);
        assert_eq!(BUILD.as_str(), "build");
        assert_eq!(BUILD.kind(), Program::ID);
    }

    #[test]
    fn a_static_head_is_sync_when_k_is_not() {
        // Catches PhantomData<K> instead of PhantomData<fn() -> K>, which would
        // require K: Sync for a static Head.
        struct NotSync(PhantomData<Cell<u8>>);
        impl Kind for NotSync {
            const NAME: &'static str = "test.bloomery.head.not_sync";
            const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
        }
        static HEAD: Head<NotSync> = Head::new("main");
        fn needs_sync<T: Sync>(_: &T) {}
        needs_sync(&HEAD);
        assert_eq!(HEAD.as_str(), "main");
    }

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        // Catches a check that swapped reasons, skipped a predicate, or
        // accepted the empty string.
        let too_long = "a".repeat(Head::<Tree>::MAX_BYTES + 1);
        let max_len = "a".repeat(Head::<Tree>::MAX_BYTES);
        let cases = [
            ("", HeadNameError::Empty, "a"),
            (too_long.as_str(), HeadNameError::TooLong, max_len.as_str()),
            (" main", HeadNameError::Whitespace, "main"),
            ("main ", HeadNameError::Whitespace, "main"),
            ("a\u{00A0}b", HeadNameError::Whitespace, "ab"),
            ("a\u{3000}b", HeadNameError::Whitespace, "ab"),
            ("a\u{2028}b", HeadNameError::Whitespace, "ab"),
            ("a\nb", HeadNameError::Control, "ab"),
            ("a\0b", HeadNameError::Control, "ab"),
            ("a\u{007F}b", HeadNameError::Control, "ab"),
            ("a\u{0080}b", HeadNameError::Control, "ab"),
            ("a\u{009F}b", HeadNameError::Control, "ab"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(RecordedHead::new(Tree::ID, reject).map(|_| ()), Err(error), "reject {reject:?}");
            assert_eq!(
                RecordedHead::new(Tree::ID, accept).expect("accepted neighbour").as_str(),
                accept,
                "accept {accept:?}"
            );
        }
    }

    #[test]
    fn the_byte_cap_is_not_a_character_cap() {
        // Catches a check that counted characters, so 64 × 'é' (128 bytes)
        // would be treated like 64 ASCII bytes, and 64 × 'é' plus one more
        // byte would sneak under a 128-character cap.
        let max = "é".repeat(64);
        assert_eq!(max.len(), Head::<Tree>::MAX_BYTES);
        assert_eq!(max.chars().count(), 64);
        assert_eq!(RecordedHead::new(Tree::ID, max.as_str()).expect("128 bytes").as_str(), max);

        let mut over = max;
        over.push('a');
        assert_eq!(over.len(), Head::<Tree>::MAX_BYTES + 1);
        assert_eq!(RecordedHead::new(Tree::ID, over.as_str()).map(|_| ()), Err(HeadNameError::TooLong));
    }

    #[test]
    fn length_is_checked_before_characters() {
        // Catches a scan that reported whitespace or control on an
        // over-long input instead of too-long.
        let spaces = " ".repeat(Head::<Tree>::MAX_BYTES + 1);
        assert_eq!(RecordedHead::new(Tree::ID, spaces.as_str()).map(|_| ()), Err(HeadNameError::TooLong));
        let tabs = "\t".repeat(Head::<Tree>::MAX_BYTES + 1);
        assert_eq!(RecordedHead::new(Tree::ID, tabs.as_str()).map(|_| ()), Err(HeadNameError::TooLong));
    }

    #[test]
    fn a_character_that_is_both_control_and_whitespace_is_control() {
        // Catches a check that tested whitespace first and reported tab
        // or newline as whitespace.
        assert_eq!(RecordedHead::new(Tree::ID, "\t").map(|_| ()), Err(HeadNameError::Control));
        assert_eq!(RecordedHead::new(Tree::ID, "\n").map(|_| ()), Err(HeadNameError::Control));
        assert_eq!(RecordedHead::new(Tree::ID, "\u{0085}").map(|_| ()), Err(HeadNameError::Control));
        assert_eq!(RecordedHead::new(Tree::ID, " ").map(|_| ()), Err(HeadNameError::Whitespace));
    }

    #[test]
    fn punctuation_case_and_decomposition_are_preserved() {
        // Catches a wrapper that folded case, NFC-normalized, or treated
        // punctuation as a path or filesystem restriction.
        for accept in ["main/head", "a.b", "foo-bar", "foo:bar", "foo_bar", "foo*bar", "Main"] {
            assert_eq!(RecordedHead::new(Tree::ID, accept).expect("punctuation and case are allowed").as_str(), accept);
        }
        let upper = RecordedHead::new(Tree::ID, "Main").expect("case is allowed");
        let lower = RecordedHead::new(Tree::ID, "main").expect("case is allowed");
        assert_ne!(upper, lower);
        assert!(upper < lower);

        let composed = RecordedHead::new(Tree::ID, "\u{00E9}").expect("composed spelling");
        let decomposed = RecordedHead::new(Tree::ID, "e\u{0301}").expect("decomposed spelling");
        assert_ne!(composed, decomposed);
        assert_eq!(composed.as_str(), "\u{00E9}");
        assert_eq!(decomposed.as_str(), "e\u{0301}");
    }

    #[test]
    fn the_scalar_reader_walks_each_utf8_width() {
        // Catches a reader that treated a 2/3/4-byte scalar as several
        // characters, or that could not round-trip already-valid str bytes.
        let text = "Aé€𝄞";
        assert_eq!("A".len(), 1);
        assert_eq!("é".len(), 2);
        assert_eq!("€".len(), 3);
        assert_eq!("𝄞".len(), 4);

        let bytes = text.as_bytes();
        let mut i = 0;
        let mut decoded = Vec::new();
        while i < bytes.len() {
            let (ch, next) = next_scalar(bytes, i);
            decoded.push(ch);
            i = next;
        }
        assert_eq!(decoded, text.chars().collect::<Vec<_>>());
        assert_eq!(next_scalar(b"A", 0), ('A', 1));
        assert_eq!(next_scalar("é".as_bytes(), 0), ('é', 2));
        assert_eq!(next_scalar("€".as_bytes(), 0), ('€', 3));
        assert_eq!(next_scalar("𝄞".as_bytes(), 0), ('𝄞', 4));
    }

    #[test]
    fn const_check_matches_ordinary_character_iteration() {
        // Catches the const UTF-8 walk disagreeing with str::chars on width,
        // control, or whitespace, so a name accepted at decode would panic
        // in a const initializer, or the reverse.
        let too_long = "a".repeat(Head::<Tree>::MAX_BYTES + 1);
        let samples = [
            "",
            "main",
            " ",
            "\t",
            "\n",
            "a\u{00A0}b",
            "a\u{3000}b",
            "a\u{2028}b",
            "a\u{0085}b",
            "é",
            "€",
            "𝄞",
            "Aé€𝄞",
            too_long.as_str(),
        ];
        for sample in samples {
            assert_eq!(check(sample), check_by_chars(sample), "parity {sample:?}");
        }
    }

    #[test]
    fn tree_and_program_may_share_the_text_main() {
        // Catches a global text-uniqueness rule that forbade independent
        // (Tree, "main") and (Program, "main") identities.
        const TREE: Head<Tree> = Head::new("main");
        const PROGRAM: Head<Program> = Head::new("main");
        assert_eq!(TREE.as_str(), PROGRAM.as_str());
        assert_ne!(TREE.kind(), PROGRAM.kind());
        assert_ne!(RecordedHead::from(&TREE), RecordedHead::from(&PROGRAM));
    }
}
