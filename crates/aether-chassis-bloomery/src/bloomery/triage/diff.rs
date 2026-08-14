//! What a repair lap actually changed, read off its unified diff.
//!
//! One product: the lap's **changed text** — every added and removed line, the
//! `@@` section heading of the hunk it sits in, and the path of the file it is
//! in. Nothing else.
//!
//! Context lines are deliberately excluded. They are what the lap did **not**
//! change, and counting them would let any edit anywhere in a file satisfy a
//! finding about anything else in that file — which is exactly the shape of the
//! dodge this triage exists to catch (an `unwrap()` → `expect()` swap in the file
//! the finding named). The section heading and the file path are included
//! because they say *where* the change is, which is the one piece of surrounding
//! text the lap chose.

/// A repair lap's diff, reduced to what the triage checks against: one entry per
/// substantively changed line, per substantive hunk's section heading, and per
/// substantively changed file's path.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChangedSurface {
    changed: Vec<String>,
}

impl ChangedSurface {
    /// Whether the lap changed anything at all after the noise rule.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// Whether `symbol` appears as a whole word anywhere in the changed text.
    #[must_use]
    pub fn mentions(&self, symbol: &str) -> bool {
        self.changed.iter().any(|line| contains_word(line, symbol))
    }
}

/// One hunk being accumulated: its section heading and its changed lines, held
/// until the hunk ends so the noise rule can judge the pair together.
#[derive(Default)]
struct Hunk {
    heading: Option<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

impl Hunk {
    /// Whether this hunk changed anything beyond whitespace.
    ///
    /// The rule: collapse every changed line's whitespace, then cancel each
    /// added line against an identical removed one. A hunk whose two sides
    /// cancel completely is a re-indent or a re-wrap — it moved text without
    /// changing it, so it touches nothing.
    fn substantive(&self) -> bool {
        let mut unmatched: Vec<String> = self.removed.iter().map(|line| collapse(line)).collect();
        for added in self.added.iter().map(|line| collapse(line)) {
            match unmatched.iter().position(|held| *held == added) {
                Some(index) => drop(unmatched.swap_remove(index)),
                None => return true,
            }
        }

        !unmatched.is_empty()
    }

    /// Drain this hunk into `surface`, if it changed anything.
    fn flush(&mut self, surface: &mut ChangedSurface, file: Option<&str>) {
        if self.substantive() {
            surface.changed.extend(file.map(str::to_owned));
            surface.changed.extend(self.heading.take());
            surface.changed.append(&mut self.added);
            surface.changed.append(&mut self.removed);
        }
        *self = Self::default();
    }
}

/// Read a unified diff into the surface the triage checks against.
///
/// Tolerant by construction: anything the parser does not recognize is skipped
/// rather than refused. A diff it reads as empty produces an empty surface, and
/// an empty surface is never used to bounce a lap — the caller passes instead.
#[must_use]
pub fn changed_surface(diff: &str) -> ChangedSurface {
    let mut surface = ChangedSurface::default();
    let mut hunk = Hunk::default();
    let mut file: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            hunk.flush(&mut surface, file.as_deref());
            file = strip_diff_prefix(path.trim());
        } else if let Some(header) = line.strip_prefix("@@") {
            hunk.flush(&mut surface, file.as_deref());
            hunk.heading = header
                .split_once("@@")
                .map(|(_, heading)| heading.trim().to_owned())
                .filter(|heading| !heading.is_empty());
        } else if let Some(added) = line.strip_prefix('+') {
            hunk.added.push(added.to_owned());
        } else if line.starts_with("--- ") {
            // The `a/` side of the same file the `+++` line names; a rename or a
            // delete is still identified by the `+++` path (`/dev/null` for a
            // delete, which strips to nothing and files no change).
        } else if let Some(removed) = line.strip_prefix('-') {
            hunk.removed.push(removed.to_owned());
        }
    }
    hunk.flush(&mut surface, file.as_deref());

    surface
}

/// The repository-relative path a `+++` line names, or `None` for `/dev/null`.
fn strip_diff_prefix(path: &str) -> Option<String> {
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix("b/").or_else(|| path.strip_prefix("a/")).unwrap_or(path).to_owned())
}

/// `line` with every whitespace run collapsed to one space and the ends trimmed
/// — the form the noise rule compares.
fn collapse(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `haystack` contains `needle` bounded by non-identifier characters on
/// both sides, so `expect` does not match inside `expected` and `Decision` does
/// not match inside `Decisions`.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        if !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char) {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether `c` continues an identifier.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
