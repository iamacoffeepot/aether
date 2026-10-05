//! `tree.grep`: the lines of the tree's files that match a regex.

use aether_bloomery_kinds::{Mode, Name, Node, Ref, Refusal, Tree};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;
use regex::{Regex, RegexBuilder};

use crate::tools::read_args;
use crate::tools::spine::{directory, leaf};
use crate::tools::view::{Family, Lines, VIEW_MAX_BYTES, Viewed, cut};

/// The hits one grep shows when it names no count.
pub const GREP_DEFAULT_HITS: usize = 100;

/// The most hits one grep shows.
pub const GREP_MAX_HITS: usize = 1000;

/// The most entries one grep visits, directories included.
pub const GREP_MAX_FILES: usize = 5000;

/// The most bytes of file text one grep reads: 16 MiB.
pub const GREP_MAX_SCANNED_BYTES: usize = 16 << 20;

/// The longest pattern one grep compiles: 1 KiB.
const GREP_MAX_PATTERN_BYTES: usize = 1 << 10;

/// The most memory a compiled pattern, and its lazy DFA, may take: 1 MiB.
const GREP_MAX_REGEX_BYTES: usize = 1 << 20;

/// The most bytes one shown hit line keeps; the rest is cut and marked `…`.
const GREP_MAX_LINE_BYTES: usize = 300;

/// What `tree.grep` is asked to find.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.grep.args")]
pub struct GrepArgs {
    /// The regex to match against each line, in Rust `regex` syntax; `(?i)`
    /// ignores case. At most 1 KiB.
    pattern: String,
    /// The file or directory to search, relative to the tree's root,
    /// `/`-separated, with no `.` or `..` segment. Leave it out to search
    /// the whole tree.
    path: Option<TreePath>,
    /// How many matching lines to show. Leave it out for 100; at most 1000.
    max_hits: Option<u32>,
}

impl GrepArgs {
    /// Find `pattern` under `path` (or the whole tree), showing at most
    /// `max_hits` matching lines (or 100).
    #[must_use]
    pub fn new(pattern: impl Into<String>, path: Option<TreePath>, max_hits: Option<u32>) -> Self {
        Self { pattern: pattern.into(), path, max_hits }
    }
}

/// The `tree.grep` program.
pub struct TreeGrep;

/// Finds the lines of the tree's UTF-8 files that match a regex, shown as
/// `<path>:<line>:<text>` in path order.
///
/// Symlinks are not followed and files that are not UTF-8 are skipped. A hit
/// line longer than 300 bytes is cut and marked `…`. The search stops at its
/// hit count, at the 64 KiB output cap, or after 5000 entries or 16 MiB of
/// text, and a closing `[...]` line says which. A pattern that does not
/// compile, or a path that names nothing to search, returns a sentence
/// saying why.
#[program]
impl Program for TreeGrep {
    const NAME: &'static str = "tree.grep";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Find the lines of the tree's files that match a regex.";
    type Input = Tooled<GrepArgs>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        grep(&mut env, input.tree(), input.args(), Family::Tree, Budget::FULL).await
    }
}

/// How much of the tree one grep may visit.
#[derive(Debug, Clone, Copy)]
pub(super) struct Budget {
    /// The most entries visited, directories included.
    files: usize,
    /// The most bytes of file text read.
    bytes: usize,
}

impl Budget {
    /// What a grep over a whole tree may visit.
    pub(super) const FULL: Self = Self { files: GREP_MAX_FILES, bytes: GREP_MAX_SCANNED_BYTES };
}

/// One grep of `root` for `args` within `budget`, with hints naming
/// `family`'s tools.
///
/// # Errors
///
/// The [`Refusal`] of a directory or file the store cannot give.
pub(super) async fn grep(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    args: Ref<GrepArgs>,
    family: Family,
    budget: Budget,
) -> Result<Viewed, Refusal> {
    let args = match read_args(env, args).await? {
        Ok(args) => args,
        Err(invalid) => return Ok(Viewed::new(format!("{invalid}."))),
    };
    if args.pattern.len() > GREP_MAX_PATTERN_BYTES {
        return Ok(Viewed::new("The pattern is longer than 1 KiB."));
    }
    let regex = match RegexBuilder::new(&args.pattern)
        .size_limit(GREP_MAX_REGEX_BYTES)
        .dfa_size_limit(GREP_MAX_REGEX_BYTES)
        .build()
    {
        Ok(regex) => regex,
        Err(error) => return Ok(Viewed::new(format!("The pattern does not compile: {error}"))),
    };

    let start = match &args.path {
        None => match directory(env, root, None, family).await? {
            Ok((_, root)) => entries("", root.entries()),
            Err(message) => return Ok(Viewed::new(message)),
        },
        Some(path) => match leaf(env, root, path).await? {
            Ok(file @ (Node::File(_) | Node::Executable(_))) => vec![(path.as_str().to_owned(), file)],
            Ok(Node::Directory(dir)) => entries(path.as_str(), env.read(dir).await?.entries()),
            Ok(Node::Symlink(target)) => {
                let (path, target) = (path.as_str(), target.as_str());
                return Ok(Viewed::new(format!("{path} is a symlink to {target}; grep that path instead.")));
            }
            Err(blocked) => return Ok(Viewed::new(blocked.describe())),
        },
    };
    let scope = args.path.as_ref().map_or("the tree", TreePath::as_str);
    let max_hits = args.max_hits.map_or(GREP_DEFAULT_HITS, |hits| usize::try_from(hits).unwrap_or(usize::MAX));
    search(env, scope, start, &regex, max_hits.clamp(1, GREP_MAX_HITS), budget).await
}

/// The entries of a directory shown under `prefix`, in name order.
fn entries<'a>(prefix: &str, listed: impl IntoIterator<Item = (&'a Name, &'a Node)>) -> Vec<(String, Node)> {
    listed
        .into_iter()
        .map(|(name, node)| {
            let shown = if prefix.is_empty() {
                name.as_str().to_owned()
            } else {
                format!("{prefix}/{}", name.as_str())
            };
            (shown, node.clone())
        })
        .collect()
}

/// Why a search stopped before it visited every entry.
enum Stop {
    Hits(usize),
    Output(usize),
    Entries(usize),
    Bytes(usize),
}

impl Stop {
    /// The closing line of a search that stopped.
    fn marker(&self) -> String {
        match self {
            Self::Hits(hits) => format!("[stopped at {hits} hits; raise max_hits or narrow path]"),
            Self::Output(hits) => {
                format!("[stopped at the 64 KiB output cap after {hits} hits; narrow path or pattern]")
            }
            Self::Entries(entries) => format!("[stopped after scanning {entries} entries; narrow path]"),
            Self::Bytes(bytes) => format!("[stopped after scanning {bytes} bytes; narrow path]"),
        }
    }
}

/// The lines matching `regex` in `start` and everything below it, walked in
/// preorder by name with an explicit stack, at most `max_hits` of them,
/// within `budget`. `scope` names what was searched when nothing matches.
///
/// # Errors
///
/// The [`Refusal`] of a directory or file the store cannot give.
async fn search(
    env: &mut Env<Async>,
    scope: &str,
    start: Vec<(String, Node)>,
    regex: &Regex,
    max_hits: usize,
    budget: Budget,
) -> Result<Viewed, Refusal> {
    let mut stack: Vec<(String, Node)> = start.into_iter().rev().collect();
    let (mut hits, mut visited, mut scanned) = (0, 0, 0);
    let mut lines = Lines::new(VIEW_MAX_BYTES);
    let mut stop = None;
    'walk: while let Some((path, node)) = stack.pop() {
        if visited >= budget.files {
            stop = Some(Stop::Entries(visited));
            break;
        }
        if scanned >= budget.bytes {
            stop = Some(Stop::Bytes(scanned));
            break;
        }
        visited += 1;
        let blob = match node {
            Node::Directory(dir) => {
                stack.extend(entries(&path, env.read(dir).await?.entries()).into_iter().rev());
                continue;
            }
            Node::File(blob) | Node::Executable(blob) => blob,
            Node::Symlink(_) => continue,
        };
        let payload = env.read_payload(blob.erase()).await?;
        scanned += payload.len();
        let Ok(text) = String::from_utf8(payload) else {
            continue;
        };
        for (index, line) in text.lines().enumerate().filter(|(_, line)| regex.is_match(line)) {
            if hits == max_hits {
                stop = Some(Stop::Hits(hits));
                break 'walk;
            }
            if !lines.push(&format!("{path}:{}:{}", index + 1, cut(line, GREP_MAX_LINE_BYTES))) {
                stop = Some(Stop::Output(hits));
                break 'walk;
            }
            hits += 1;
        }
    }

    let Some(stop) = stop else {
        if lines.is_empty() {
            return Ok(Viewed::new(format!("No match for `{}` in {scope}.", regex.as_str())));
        }
        return Ok(lines.finish(String::new));
    };
    lines.stop();
    Ok(lines.finish(|| stop.marker()))
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Mode, Refusal};
    use aether_bloomery_program::{Async, Env, Program, Tooled, program};

    use super::{Budget, GrepArgs, TreeGrep, grep};
    use crate::session::fixture::{SmallTree, path, run_async};
    use crate::tools::view::{Family, Viewed};

    /// `tree.grep` over a two-entry budget.
    struct TwoEntryGrep;

    /// Finds lines like `tree.grep`, visiting at most two entries.
    #[program]
    impl Program for TwoEntryGrep {
        const NAME: &'static str = "test.tree.grep.two";
        const MODE: Mode = Mode::Pure;
        const INTENT: &'static str = "Grep within a two-entry budget.";
        type Input = Tooled<GrepArgs>;
        type Result = Viewed;

        async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
            grep(&mut env, input.tree(), input.args(), Family::Tree, Budget { files: 2, bytes: usize::MAX }).await
        }
    }

    /// The text `tree.grep` returns for `args` over the small tree.
    fn grepped(small: &SmallTree, args: &GrepArgs) -> String {
        let (input, closure) = small.call(args);
        let (viewed, _) = run_async::<TreeGrep>(&input, closure).expect("a grep is a result");
        viewed.text().to_owned()
    }

    #[test]
    fn hits_come_in_path_order_and_skip_binaries_and_symlinks() {
        // Catches a walk out of name order, a hit numbered off by one, a symlink followed, a non-UTF-8 file
        // refused, and a path that searches the wrong subtree.
        let small = SmallTree::new();
        assert_eq!(grepped(&small, &GrepArgs::new("smelt", None, None)), "run:2:smelt\nsrc/lib.rs:1:pub fn smelt() {}");
        assert_eq!(
            grepped(&small, &GrepArgs::new("(?i)BLOOMERY|fn", None, None)),
            "README:1:# Bloomery\nsrc/lib.rs:1:pub fn smelt() {}"
        );
        assert_eq!(
            grepped(&small, &GrepArgs::new(r"\w+\(\)", Some(path("src")), None)),
            "src/lib.rs:1:pub fn smelt() {}"
        );
        assert_eq!(grepped(&small, &GrepArgs::new("smelt", Some(path("run")), None)), "run:2:smelt");
        assert_eq!(grepped(&small, &GrepArgs::new("iron", Some(path("src")), None)), "No match for `iron` in src.");
    }

    #[test]
    fn a_grep_stops_at_its_hit_count_and_its_budget_and_says_which() {
        // Catches a walk that ignores its hit count or its budget, and a stop left unmarked.
        let small = SmallTree::new();
        assert_eq!(
            grepped(&small, &GrepArgs::new("smelt", None, Some(1))),
            "run:2:smelt\n[stopped at 1 hits; raise max_hits or narrow path]"
        );

        let (input, closure) = small.call(&GrepArgs::new("Bloomery|smelt", None, None));
        let (viewed, _) = run_async::<TwoEntryGrep>(&input, closure).expect("a grep is a result");
        assert_eq!(viewed.text(), "README:1:# Bloomery\n[stopped after scanning 2 entries; narrow path]");
    }

    #[test]
    fn a_pattern_or_path_it_cannot_search_is_a_result_saying_why() {
        // Catches a model's mistake refused as a fault, which would end the session, and a pattern compiled past
        // its size limit.
        let small = SmallTree::new();
        for (args, start) in [
            (GrepArgs::new("(", None, None), "The pattern does not compile"),
            (GrepArgs::new(r"(?:\w{1000}){1000}", None, None), "The pattern does not compile"),
            (GrepArgs::new("a".repeat(1025), None, None), "The pattern is longer than 1 KiB."),
            (GrepArgs::new("a", Some(path("link")), None), "link is a symlink to README;"),
            (GrepArgs::new("a", Some(path("src/missing")), None), "Nothing is at src/missing."),
        ] {
            let text = grepped(&small, &args);
            assert!(text.starts_with(start), "{args:?}: {text}");
        }
    }
}
