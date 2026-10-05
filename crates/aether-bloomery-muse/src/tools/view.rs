//! What a tool that only reads the tree returns, and the capped text it is
//! built from.

use std::borrow::Cow;

/// The most bytes of text one read-only tool call returns: 64 KiB, the
/// closing truncation line included.
pub const VIEW_MAX_BYTES: usize = 64 * 1024;

/// The bytes kept back from a text's budget for its closing truncation line.
const MARKER_BYTES: usize = 256;

/// Which tree a read-only tool reads, for the hints that name its sibling.
///
/// A hint tells the model which tool to call next, and the model must stay in
/// the tree it is reading, so the sibling's name follows the family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// The session's own tree: `tree.list`, `tree.read`, `tree.grep`.
    Tree,
    /// The vendored crate sources: `vendor.list`, `vendor.read`,
    /// `vendor.grep`.
    Vendor,
}

impl Family {
    /// The name of this family's listing tool.
    #[must_use]
    pub const fn list(self) -> &'static str {
        match self {
            Self::Tree => "tree.list",
            Self::Vendor => "vendor.list",
        }
    }

    /// The name of this family's reading tool.
    #[must_use]
    pub const fn read(self) -> &'static str {
        match self {
            Self::Tree => "tree.read",
            Self::Vendor => "vendor.read",
        }
    }
}

/// A read-only tool's result: the text it read from the tree.
///
/// The read-only counterpart of `Edited`: the loop binds a later call to the
/// tree an `Edited` result carries, and a `Viewed` result carries none, so
/// the session's tree stays as it was. A read the tool could not make is a
/// `Viewed` too, whose text says why.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.viewed")]
pub struct Viewed {
    /// What the call read, one entry, line, or match per line, with a closing
    /// `[...]` line when the output was cut; or one sentence on why it read
    /// nothing.
    text: String,
}

impl Viewed {
    /// A result whose text is `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// What the call read, or why it read nothing.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Lines joined by `\n` up to a byte budget; the first line that would go
/// over it, and every line after, is cut.
pub struct Lines {
    text: String,
    budget: usize,
    cut: bool,
}

impl Lines {
    /// Lines whose text, with a closing truncation line, fits `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self { text: String::new(), budget: max_bytes.saturating_sub(MARKER_BYTES), cut: false }
    }

    /// Add `line`, or refuse it once it would go over the budget, marking the
    /// text cut.
    pub fn push(&mut self, line: &str) -> bool {
        let separator = usize::from(!self.text.is_empty());
        if self.cut || self.text.len() + separator + line.len() > self.budget {
            self.cut = true;
            return false;
        }
        if separator == 1 {
            self.text.push('\n');
        }
        self.text.push_str(line);
        true
    }

    /// Mark the text cut for a reason other than its byte budget.
    pub fn stop(&mut self) {
        self.cut = true;
    }

    /// Whether no line was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text, closed with the line `marker` gives when anything was cut.
    #[must_use]
    pub fn finish(mut self, marker: impl FnOnce() -> String) -> Viewed {
        if self.cut {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str(&marker());
        }
        Viewed { text: self.text }
    }
}

/// `line` cut to at most `max_bytes` on a char boundary, marked with `…`
/// when anything was cut.
#[must_use]
pub fn cut(line: &str, max_bytes: usize) -> Cow<'_, str> {
    if line.len() <= max_bytes {
        return Cow::Borrowed(line);
    }
    Cow::Owned(format!("{}…", &line[..line.floor_char_boundary(max_bytes)]))
}
