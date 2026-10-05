//! Where a text occurs in a file, and where an old text that does not occur
//! stops matching it.
//!
//! A hint needs an anchor: the longest prefix of the old text that occurs in
//! the file, or failing that its longest suffix, and only when that part
//! occurs exactly once. A part that occurs several times names no one place,
//! so it gives no hint.

use std::borrow::Cow;

use crate::tools::view::cut;

/// The most bytes of a line a hint shows on either side of the anchor.
const SHOWN_MAX_BYTES: usize = 60;

/// Why a text has no one occurrence.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Matches {
    None,
    Several,
}

/// The byte offset of the one occurrence of the non-empty `needle` in `text`.
/// A second occurrence may overlap the first: `aa` occurs twice in `aaa`.
///
/// # Errors
///
/// [`Matches::None`] when `needle` does not occur, and [`Matches::Several`]
/// when it occurs more than once.
pub(super) fn occurrence(text: &str, needle: &str) -> Result<usize, Matches> {
    let first = text.find(needle).ok_or(Matches::None)?;
    let next = first + needle.chars().next().map_or(1, char::len_utf8);
    if text[next..].contains(needle) {
        return Err(Matches::Several);
    }
    Ok(first)
}

/// Where `old`, which does not occur in `text`, stops matching it: the line
/// its one anchor sits on and how the file and `old` each go on from there.
/// None when neither end of `old` occurs exactly once.
pub(super) fn hint(text: &str, old: &str) -> Option<String> {
    leading(text, old).or_else(|| trailing(text, old))
}

/// The hint anchored on the longest prefix of `old` that occurs in `text`.
fn leading(text: &str, old: &str) -> Option<String> {
    let matched = longest_prefix(text, old);
    if matched == 0 {
        return None;
    }
    let at = occurrence(text, &old[..matched]).ok()? + matched;

    let line = 1 + text[..at].matches('\n').count();
    let file = shown(cut(first_line(&text[at..]), SHOWN_MAX_BYTES), "(end of line)");
    let wanted = shown(cut(first_line(&old[matched..]), SHOWN_MAX_BYTES), "(end of line)");
    let matched = bytes(matched);
    Some(format!(
        "Its first {matched} occur once, ending on line {line}; there the file continues with:\n{file}\nbut the old \
         text continues with:\n{wanted}"
    ))
}

/// The hint anchored on the longest suffix of `old` that occurs in `text`.
fn trailing(text: &str, old: &str) -> Option<String> {
    let matched = longest_suffix(text, old);
    if matched == 0 {
        return None;
    }
    let split = old.len() - matched;
    let at = occurrence(text, &old[split..]).ok()?;

    let line = 1 + text[..at].matches('\n').count();
    let file = shown(cut_start(last_line(&text[..at]), SHOWN_MAX_BYTES), "(start of line)");
    let wanted = shown(cut_start(last_line(&old[..split]), SHOWN_MAX_BYTES), "(start of line)");
    let matched = bytes(matched);
    Some(format!(
        "Its last {matched} occur once, starting on line {line}; before them the file has:\n{file}\nbut the old text \
         has:\n{wanted}"
    ))
}

/// The length of the longest prefix of `old`, on a char boundary, that
/// occurs in `text`; 0 when not even its first char does.
///
/// A prefix that occurs has every shorter prefix occurring, so the length is
/// found by bisection: `old[..low]` always occurs and `old[..high]` never
/// does, since `old` itself does not occur.
fn longest_prefix(text: &str, old: &str) -> usize {
    let (mut low, mut high) = (0, old.len());
    loop {
        let middle = old.floor_char_boundary(low + (high - low) / 2);
        let probe = old.ceil_char_boundary(middle.max(low + 1));
        if probe >= high {
            return low;
        }
        if text.contains(&old[..probe]) {
            low = probe;
        } else {
            high = probe;
        }
    }
}

/// The length of the longest suffix of `old`, on a char boundary, that
/// occurs in `text`; 0 when not even its last char does.
///
/// The mirror of [`longest_prefix`]: `old[good..]` always occurs and
/// `old[bad..]` never does.
fn longest_suffix(text: &str, old: &str) -> usize {
    let (mut bad, mut good) = (0, old.len());
    loop {
        let middle = old.ceil_char_boundary(bad + (good - bad) / 2);
        let probe = old.floor_char_boundary(middle.min(good - 1));
        if probe <= bad {
            return old.len() - good;
        }
        if text.contains(&old[probe..]) {
            good = probe;
        } else {
            bad = probe;
        }
    }
}

/// `text` up to its first line break.
fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

/// `text` after its last line break.
fn last_line(text: &str) -> &str {
    text.rsplit('\n').next().unwrap_or_default()
}

/// The last `max_bytes` of `line` on a char boundary, marked with a leading
/// `…` when anything was cut.
fn cut_start(line: &str, max_bytes: usize) -> Cow<'_, str> {
    if line.len() <= max_bytes {
        return Cow::Borrowed(line);
    }
    Cow::Owned(format!("…{}", &line[line.ceil_char_boundary(line.len() - max_bytes)..]))
}

/// `part`, or `empty` when there is nothing of it to show.
fn shown<'a>(part: Cow<'a, str>, empty: &'a str) -> Cow<'a, str> {
    if part.is_empty() {
        return Cow::Borrowed(empty);
    }
    part
}

/// A count of bytes, as a hint spells it.
fn bytes(count: usize) -> String {
    match count {
        1 => "1 byte".to_owned(),
        _ => format!("{count} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::hint;

    /// A file whose third line is the tail of a raw string holding escaped
    /// quotes and six closing braces, the line a session miscounted.
    const FILE: &str = concat!(
        "// é smelts\n",
        "    ENDED.replace(DONE_ARGUMENTS, escaped)\n",
        r##"\"ending\": {{\"{variant}\": {{\"{field}\": \"{text}\"}}}}}}"#))"##,
        "\n}\n",
    );

    #[test]
    fn a_miss_past_a_unique_prefix_names_the_line_and_both_continuations() {
        // Catches an off-by-one in the prefix search or the line count, and the file's and the old text's sides
        // swapped: the old text has five closing braces where the line has six.
        let old = r##"\"ending\": {{\"{variant}\": {{\"{field}\": \"{text}\"}}}}}"#))"##;
        let expected = concat!(
            "Its first 59 bytes occur once, ending on line 3; there the file continues with:\n",
            "}\"#))\n",
            "but the old text continues with:\n",
            "\"#))",
        );
        assert_eq!(hint(FILE, old).as_deref(), Some(expected));
    }

    #[test]
    fn a_miss_before_a_unique_suffix_names_the_line_and_both_lead_ins() {
        // Catches the suffix search reading the wrong side of its anchor, and a lead-in that runs past its line.
        let old = "BEGAN.replace(DONE_ARGUMENTS, escaped)\n";
        let expected = concat!(
            "Its last 34 bytes occur once, starting on line 2; before them the file has:\n",
            "    ENDED\n",
            "but the old text has:\n",
            "BEGAN",
        );
        assert_eq!(hint(FILE, old).as_deref(), Some(expected));

        let expected = "Its last 2 bytes occur once, starting on line 4; before them the file has:\n(start of line)\nbut \
                        the old text has:\nq";
        assert_eq!(hint(FILE, "q}\n").as_deref(), Some(expected));
    }

    #[test]
    fn a_miss_whose_ends_each_occur_twice_gives_no_hint() {
        // Catches a hint that points at one of several candidate lines.
        let text = "let a = smelt();\nlet b = smelt();\n";
        assert_eq!(hint(text, "let c = smelt();"), None);
        assert_eq!(hint(text, "iron"), None);
    }

    #[test]
    fn a_miss_inside_a_multibyte_char_stops_on_a_char_boundary() {
        // Catches a probe that slices inside a char, which would panic: `é` and `è` share their first byte.
        let text = "one café here\n";
        let leading = "Its first 3 bytes occur once, ending on line 1; there the file continues with:\né here\nbut the \
                       old text continues with:\nè there";
        assert_eq!(hint(text, "cafè there").as_deref(), Some(leading));

        let trailing = "Its last 5 bytes occur once, starting on line 1; before them the file has:\none café\nbut the old \
                        text has:\nè";
        assert_eq!(hint(text, "è here").as_deref(), Some(trailing));
    }
}
