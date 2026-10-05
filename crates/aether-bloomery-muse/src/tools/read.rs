//! `tree.read`: a window of numbered lines from one file.

use aether_bloomery_kinds::{Mode, Node, Refusal, Tree};
use aether_bloomery_program::{Async, Env, Program, Tooled, program};
use aether_bloomery_workspace::TreePath;
use aether_data::Ref;

use crate::tools::read_args;
use crate::tools::spine::leaf;
use crate::tools::view::{Family, Lines, VIEW_MAX_BYTES, Viewed, cut};

/// The lines one read shows when it names no count.
pub const READ_DEFAULT_LINES: usize = 400;

/// The most lines one read shows.
pub const READ_MAX_LINES: usize = 2000;

/// The most bytes one shown line keeps; the rest is cut and marked `…`.
const READ_MAX_LINE_BYTES: usize = 2000;

/// What `tree.read` is asked to read.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.tree.read.args")]
pub struct ReadArgs {
    /// The file to read, relative to the tree's root, `/`-separated, with no
    /// `.` or `..` segment.
    path: TreePath,
    /// The 1-based line to start at. Leave it out to start at line 1.
    from_line: Option<u32>,
    /// How many lines to show. Leave it out for 400; at most 2000.
    lines: Option<u32>,
}

impl ReadArgs {
    /// Read the file at `path` from line `from_line` (or 1), showing `lines`
    /// lines (or 400).
    #[must_use]
    pub const fn new(path: TreePath, from_line: Option<u32>, lines: Option<u32>) -> Self {
        Self { path, from_line, lines }
    }
}

/// The `tree.read` program.
pub struct TreeRead;

/// Reads a window of a UTF-8 file of the tree as numbered lines,
/// `<n>\t<line>`, 1-based.
///
/// A line longer than 2000 bytes is cut and marked `…`. When the window
/// stops before the file's end, at its line count or at the 64 KiB output
/// cap, a closing `[...]` line gives the `from_line` to read on from. A path
/// that names no text file returns a sentence saying why.
#[program]
impl Program for TreeRead {
    const NAME: &'static str = "tree.read";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read numbered lines from one file of the tree.";
    type Input = Tooled<ReadArgs>;
    type Result = Viewed;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        read(&mut env, input.tree(), input.args(), Family::Tree).await
    }
}

/// The window `args` asks of a file of `root`, with hints naming `family`'s
/// tools.
///
/// # Errors
///
/// The [`Refusal`] of a directory or file the store cannot give.
pub(super) async fn read(
    env: &mut Env<Async>,
    root: Ref<Tree>,
    args: Ref<ReadArgs>,
    family: Family,
) -> Result<Viewed, Refusal> {
    let args = match read_args(env, args).await? {
        Ok(args) => args,
        Err(invalid) => return Ok(Viewed::new(format!("{invalid}."))),
    };
    let path = args.path.as_str();
    let blob = match leaf(env, root, &args.path).await? {
        Ok(Node::File(blob) | Node::Executable(blob)) => blob,
        Ok(Node::Directory(_)) => return Ok(Viewed::new(format!("{path} is a directory; use {}.", family.list()))),
        Ok(Node::Symlink(target)) => {
            return Ok(Viewed::new(format!("{path} is a symlink to {}.", target.as_str())));
        }
        Err(blocked) => return Ok(Viewed::new(blocked.describe())),
    };
    let Ok(text) = String::from_utf8(env.read_payload(blob.erase()).await?) else {
        return Ok(Viewed::new(format!("{path} is not UTF-8 text.")));
    };

    let from_line = args.from_line.map_or(1, count).max(1);
    let lines = args.lines.map_or(READ_DEFAULT_LINES, count);
    Ok(window(&text, from_line, lines).unwrap_or_else(|total| {
        Viewed::new(match total {
            0 => format!("{path} is empty."),
            1 => format!("{path} has 1 line; from_line {from_line} is past the end."),
            _ => format!("{path} has {total} lines; from_line {from_line} is past the end."),
        })
    }))
}

/// A count the model wrote, as a `usize`.
fn count(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Up to `lines` numbered lines of `text` from the 1-based `from_line`,
/// `lines` clamped to 1 through [`READ_MAX_LINES`], within
/// [`VIEW_MAX_BYTES`], closed with where to read on when the window stops
/// before the end.
///
/// # Errors
///
/// The text's line count when `from_line` is past its last line.
fn window(text: &str, from_line: usize, lines: usize) -> Result<Viewed, usize> {
    let total = text.lines().count();
    if from_line > total {
        return Err(total);
    }
    let mut shown = Lines::new(VIEW_MAX_BYTES);
    let last = text
        .lines()
        .enumerate()
        .skip(from_line - 1)
        .take(lines.clamp(1, READ_MAX_LINES))
        .take_while(|(index, line)| shown.push(&format!("{}\t{}", index + 1, cut(line, READ_MAX_LINE_BYTES))))
        .last()
        .map_or(from_line - 1, |(index, _)| index + 1);
    if last < total {
        shown.stop();
    }
    Ok(shown.finish(|| format!("[showing lines {from_line}-{last} of {total}; read on with from_line {}]", last + 1)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{READ_MAX_LINES, ReadArgs, TreeRead, window};
    use crate::session::fixture::{SmallTree, path, run_async};
    use crate::tools::view::VIEW_MAX_BYTES;

    /// The text of the window, or the line count past whose end it starts.
    fn windowed(text: &str, from_line: usize, lines: usize) -> Result<String, usize> {
        window(text, from_line, lines).map(|viewed| viewed.text().to_owned())
    }

    #[test]
    fn a_window_numbers_its_lines_and_says_where_to_read_on() {
        // Catches an off-by-one in the numbering or in the marker's from_line, a marker missing on a cut window
        // or present on a whole one, and a count past the clamp.
        let text = "a\nb\nc\nd\n";
        assert_eq!(windowed(text, 1, 400), Ok("1\ta\n2\tb\n3\tc\n4\td".into()));
        assert_eq!(windowed(text, 2, 2), Ok("2\tb\n3\tc\n[showing lines 2-3 of 4; read on with from_line 4]".into()));
        assert_eq!(windowed(text, 4, 1), Ok("4\td".into()));
        assert_eq!(windowed(text, 5, 1), Err(4));

        let long = (1..=READ_MAX_LINES + 5).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let clamped = windowed(&long, 1, usize::MAX).expect("a window");
        let marker =
            format!("[showing lines 1-{READ_MAX_LINES} of {}; read on with from_line 2001]", READ_MAX_LINES + 5);
        assert!(clamped.ends_with(&format!("2000\t2000\n{marker}")), "{clamped}");
    }

    #[test]
    fn a_window_stops_at_the_byte_cap_on_a_line_boundary() {
        // Catches a window that overruns the output cap, splits a line at the cap, or names the wrong next line.
        let line = "x".repeat(99);
        let text = format!("{line}\n").repeat(1000);
        let shown = windowed(&text, 1, READ_MAX_LINES).expect("a window");
        assert!(shown.len() <= VIEW_MAX_BYTES, "{}", shown.len());

        let (body, marker) = shown.rsplit_once('\n').expect("a marker line");
        let last_line = body.lines().last().expect("a shown line");
        let (number, rest) = last_line.split_once('\t').expect("a numbered line");
        assert_eq!(rest, line, "the last shown line is whole");
        let next = number.parse::<usize>().expect("a number") + 1;
        assert_eq!(marker, format!("[showing lines 1-{number} of 1000; read on with from_line {next}]"));
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary() {
        // Catches a cut inside a multibyte char, which would panic, and a cut line left unmarked.
        let text = "é".repeat(1500);
        let shown = windowed(&text, 1, 1).expect("a window");
        assert_eq!(shown, format!("1\t{}…", "é".repeat(1000)));
    }

    #[test]
    fn a_read_returns_numbered_text_and_caps_its_window() {
        // Catches a read that skips an executable, and one that ignores its arguments.
        let small = SmallTree::new();
        for (args, text) in [
            (ReadArgs::new(path("run"), None, None), "1\t#!/bin/sh\n2\tsmelt"),
            (ReadArgs::new(path("run"), Some(2), None), "2\tsmelt"),
            (
                ReadArgs::new(path("run"), None, Some(1)),
                "1\t#!/bin/sh\n[showing lines 1-1 of 2; read on with from_line 2]",
            ),
            (ReadArgs::new(path("README"), Some(3), None), "README has 1 line; from_line 3 is past the end."),
        ] {
            let (input, closure) = small.call(&args);
            let (viewed, _) = run_async::<TreeRead>(&input, closure).expect("a read is a result");
            assert_eq!(viewed.text(), text, "{args:?}");
        }
    }

    #[test]
    fn a_path_that_names_no_text_file_is_a_result_saying_why() {
        // Catches a model's mistake refused as a fault, which would end the session.
        let small = SmallTree::new();
        for (at, text) in [
            ("src", "src is a directory; use tree.list."),
            ("link", "link is a symlink to README."),
            ("blob.bin", "blob.bin is not UTF-8 text."),
            ("src/missing.rs", "Nothing is at src/missing.rs."),
        ] {
            let (input, closure) = small.call(&ReadArgs::new(path(at), None, None));
            let (viewed, _) = run_async::<TreeRead>(&input, closure).expect("a failed read is a result");
            assert_eq!(viewed.text(), text, "{at}");
        }

        let (input, closure) = small.call_json::<ReadArgs>(&json!({ "path": "../x" }));
        let (viewed, _) = run_async::<TreeRead>(&input, closure).expect("invalid arguments are a result");
        assert!(viewed.text().starts_with("The arguments are invalid"), "{}", viewed.text());
    }
}
