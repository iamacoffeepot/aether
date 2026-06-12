//! ADR-0103 §5 SFZ-subset parser for sampled instrument banks.
//!
//! A pure function of text: take an `.sfz` file's contents and return a
//! [`BankSpec`] — the flat list of regions with their key / velocity
//! ranges and root pitch, each pointing at a sample path resolved against
//! the bank's `default_path`. No I/O: the audio cap fetches the `.sfz`
//! bytes and every referenced sample through `aether.fs` (ADR-0103 §2),
//! so namespace resolution stays single-sourced; this module only turns
//! the already-fetched text into structure.
//!
//! The subset is deliberately small (ADR-0103 §5). The format is
//! `opcode=value` runs under `<header>` tags:
//!
//! - Headers: `<control>` (carries `default_path`), `<group>`, `<region>`.
//!   Group opcodes are inherited by every following region until the next
//!   `<group>`.
//! - Opcodes: `sample`, `lokey` / `hikey`, `pitch_keycenter`,
//!   `lovel` / `hivel`. The `sample` value runs to the end of the line so
//!   filenames may carry spaces; the rest are single whitespace-delimited
//!   tokens.
//! - `//` line comments are stripped.
//!
//! Everything outside that set — the loop opcodes (`loop_start` /
//! `loop_end` / `loop_mode`, parked for #1682), envelope opcodes, unknown
//! headers — warns and is ignored, so a real-world sample set loads rather
//! than failing on the first opcode past the subset.

use std::error::Error;
use std::fmt;

/// One playable region of a bank: a sample, the inclusive MIDI key range
/// it covers, the inclusive velocity range it answers to, and the root
/// pitch the sample was recorded at (so the voice repitches by
/// `2^((pitch − pitch_keycenter) / 12)`). `sample` is already resolved
/// against the bank's `default_path`; the cap joins it with the `.sfz`
/// file's own directory to address the WAV through `aether.fs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfzRegion {
    pub sample: String,
    pub lokey: u8,
    pub hikey: u8,
    pub lovel: u8,
    pub hivel: u8,
    pub pitch_keycenter: u8,
}

/// A parsed bank: the resolved region list. The referenced sample paths
/// (deduplicated) are what the cap fans out `aether.fs.read`s for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankSpec {
    pub regions: Vec<SfzRegion>,
}

impl BankSpec {
    /// The distinct sample paths the regions reference, in first-seen
    /// order. The cap fetches each exactly once and shares the decoded
    /// PCM across every region that names it.
    #[must_use]
    pub fn sample_paths(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for region in &self.regions {
            if !seen.iter().any(|s: &String| s == &region.sample) {
                seen.push(region.sample.clone());
            }
        }
        seen
    }
}

/// Why parsing an `.sfz` failed (ADR-0103 §5). Surfaced to the requester
/// as the `Err` arm of `LoadInstrumentResult` — a malformed bank comes
/// back loud rather than logged-and-dropped, matching the rest of the
/// load path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SfzError {
    /// A header tag opened with `<` but never closed with `>` on the same
    /// line — a truncated or hand-corrupted file. Carries the offending
    /// fragment.
    MalformedHeader(String),
    /// The file parsed but yielded no playable region (no `<region>` with
    /// a `sample`), so there is nothing to load.
    NoRegions,
}

impl fmt::Display for SfzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedHeader(frag) => write!(f, "malformed SFZ header: {frag}"),
            Self::NoRegions => write!(f, "SFZ defines no playable regions"),
        }
    }
}

impl Error for SfzError {}

/// SFZ defaults for an opcode left unset on a region (the format's
/// documented defaults).
const DEFAULT_LOKEY: u8 = 0;
const DEFAULT_HIKEY: u8 = 127;
const DEFAULT_LOVEL: u8 = 0;
const DEFAULT_HIVEL: u8 = 127;
const DEFAULT_PITCH_KEYCENTER: u8 = 60;

/// Which header the parser is currently accumulating opcodes under.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Section {
    /// Outside any region/group/control — the implicit top before the
    /// first header, or under an unknown header whose opcodes are ignored.
    None,
    Control,
    Group,
    Region,
}

/// The mutable opcode set a region inherits from its group and then
/// overrides. `sample` is `Option` because a region without one is not
/// playable (skipped with a warn).
#[derive(Clone)]
struct OpcodeSet {
    sample: Option<String>,
    lokey: u8,
    hikey: u8,
    lovel: u8,
    hivel: u8,
    pitch_keycenter: u8,
}

impl OpcodeSet {
    fn defaults() -> Self {
        Self {
            sample: None,
            lokey: DEFAULT_LOKEY,
            hikey: DEFAULT_HIKEY,
            lovel: DEFAULT_LOVEL,
            hivel: DEFAULT_HIVEL,
            pitch_keycenter: DEFAULT_PITCH_KEYCENTER,
        }
    }
}

/// Parse an `.sfz` file's text into a [`BankSpec`] (ADR-0103 §5). Pure:
/// no I/O, no allocation beyond the returned regions. Group opcodes are
/// inherited by following regions; `default_path` from `<control>`
/// prefixes every region's `sample`. Opcodes outside the subset (loop
/// points, envelopes, …) warn and are ignored so real sample sets load.
pub fn parse_sfz(text: &str) -> Result<BankSpec, SfzError> {
    let mut default_path = String::new();
    let mut group = OpcodeSet::defaults();
    let mut section = Section::None;
    // The region currently being accumulated, flushed at the next header
    // or at end of input.
    let mut current: Option<OpcodeSet> = None;
    let mut regions: Vec<SfzRegion> = Vec::new();

    for raw_line in text.lines() {
        let line = strip_comment(raw_line);
        let mut rest = line.trim_start();
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('<') {
                // A header tag. Find its close on this line.
                let Some(close) = after.find('>') else {
                    return Err(SfzError::MalformedHeader(rest.to_owned()));
                };
                let tag = &after[..close];
                // Flush any region in progress before the section changes.
                flush_region(&mut current, &mut regions);
                match tag {
                    "control" => section = Section::Control,
                    "group" => {
                        group = OpcodeSet::defaults();
                        section = Section::Group;
                    }
                    "region" => {
                        current = Some(group.clone());
                        section = Section::Region;
                    }
                    other => {
                        tracing::warn!(
                            target: "aether_substrate::audio",
                            header = other,
                            "sfz: ignoring unsupported header",
                        );
                        section = Section::None;
                    }
                }
                rest = after[close + 1..].trim_start();
                continue;
            }

            // An opcode token `key=value`. The `sample` value may carry
            // spaces (filenames), so it absorbs following tokens until the
            // next opcode (`=`) or header (`<`) boundary; every other
            // opcode is the single next whitespace token.
            let (token, after_token) = next_token(rest);
            let Some((key, first_value)) = token.split_once('=') else {
                // A bare token with no `=` — stray content; skip it.
                rest = after_token;
                continue;
            };
            if key == "sample" {
                let mut value = String::from(first_value);
                let mut tail = after_token;
                loop {
                    let (peek, after_peek) = next_token(tail);
                    if peek.is_empty() || peek.starts_with('<') || peek.contains('=') {
                        break;
                    }
                    value.push(' ');
                    value.push_str(peek);
                    tail = after_peek;
                }
                apply_opcode(
                    section,
                    &mut default_path,
                    &mut group,
                    &mut current,
                    key,
                    &value,
                );
                rest = tail;
                continue;
            }
            apply_opcode(
                section,
                &mut default_path,
                &mut group,
                &mut current,
                key,
                first_value,
            );
            rest = after_token;
        }
    }
    flush_region(&mut current, &mut regions);

    // Resolve each region's sample against default_path now that the whole
    // file is parsed (a `<control>` may legally precede the regions, which
    // it does in practice, but resolve uniformly regardless of order).
    for region in &mut regions {
        region.sample = join_path(&default_path, &region.sample);
    }

    if regions.is_empty() {
        return Err(SfzError::NoRegions);
    }
    Ok(BankSpec { regions })
}

/// Strip a `//` line comment, returning the content before it.
fn strip_comment(line: &str) -> &str {
    line.find("//").map_or(line, |i| &line[..i])
}

/// Split off the next whitespace-delimited token and the remainder after
/// it (already trimmed of leading whitespace).
fn next_token(rest: &str) -> (&str, &str) {
    rest.find(char::is_whitespace)
        .map_or((rest, ""), |i| (&rest[..i], rest[i..].trim_start()))
}

/// Apply one `key=value` opcode to the current section's state. Unknown
/// opcodes (including the parked loop points) warn and are ignored.
fn apply_opcode(
    section: Section,
    default_path: &mut String,
    group: &mut OpcodeSet,
    current: &mut Option<OpcodeSet>,
    key: &str,
    value: &str,
) {
    if section == Section::Control {
        if key == "default_path" {
            value.clone_into(default_path);
        } else {
            warn_unknown_opcode(key);
        }
        return;
    }

    // Region opcodes land on the in-progress region; group opcodes on the
    // inherited set. Anything under `None` (top-of-file or unknown header)
    // is ignored.
    let target = match section {
        Section::Region => current.as_mut(),
        Section::Group => Some(&mut *group),
        Section::Control | Section::None => None,
    };
    let Some(set) = target else {
        return;
    };

    match key {
        "sample" => set.sample = Some(value.to_owned()),
        "lokey" => set.lokey = parse_key(value).unwrap_or(set.lokey),
        "hikey" => set.hikey = parse_key(value).unwrap_or(set.hikey),
        "lovel" => set.lovel = parse_key(value).unwrap_or(set.lovel),
        "hivel" => set.hivel = parse_key(value).unwrap_or(set.hivel),
        "pitch_keycenter" => {
            set.pitch_keycenter = parse_key(value).unwrap_or(set.pitch_keycenter);
        }
        _ => warn_unknown_opcode(key),
    }
}

fn warn_unknown_opcode(key: &str) {
    tracing::warn!(
        target: "aether_substrate::audio",
        opcode = key,
        "sfz: ignoring unsupported opcode",
    );
}

/// Parse a MIDI key / velocity value (0–127). Non-numeric values (note
/// names like `c4`, which the subset doesn't cover) return `None` and the
/// opcode keeps its prior value.
fn parse_key(value: &str) -> Option<u8> {
    value.parse::<u8>().ok()
}

/// Flush the in-progress region into the output list if it named a
/// sample; a region without one is not playable and is dropped with a
/// warn.
fn flush_region(current: &mut Option<OpcodeSet>, regions: &mut Vec<SfzRegion>) {
    let Some(set) = current.take() else {
        return;
    };
    let Some(sample) = set.sample else {
        tracing::warn!(
            target: "aether_substrate::audio",
            "sfz: ignoring region with no sample opcode",
        );
        return;
    };
    regions.push(SfzRegion {
        sample,
        lokey: set.lokey,
        hikey: set.hikey,
        lovel: set.lovel,
        hivel: set.hivel,
        pitch_keycenter: set.pitch_keycenter,
    });
}

/// Join a region's `sample` onto the bank's `default_path`. An empty
/// `default_path` leaves the sample as-is; otherwise the two are joined
/// with a single `/` regardless of a trailing slash on `default_path`.
fn join_path(default_path: &str, sample: &str) -> String {
    if default_path.is_empty() {
        return sample.to_owned();
    }
    format!("{}/{}", default_path.trim_end_matches('/'), sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_region_resolves_with_defaults() {
        let spec = parse_sfz("<region>\nsample=piano.wav\n").expect("parses");
        assert_eq!(spec.regions.len(), 1);
        let r = &spec.regions[0];
        assert_eq!(r.sample, "piano.wav");
        assert_eq!(r.lokey, DEFAULT_LOKEY);
        assert_eq!(r.hikey, DEFAULT_HIKEY);
        assert_eq!(r.lovel, DEFAULT_LOVEL);
        assert_eq!(r.hivel, DEFAULT_HIVEL);
        assert_eq!(r.pitch_keycenter, DEFAULT_PITCH_KEYCENTER);
    }

    #[test]
    fn key_and_velocity_ranges_parse() {
        let sfz = "<region>\nsample=a.wav lokey=48 hikey=59 lovel=0 hivel=63 pitch_keycenter=53\n";
        let spec = parse_sfz(sfz).expect("parses");
        let r = &spec.regions[0];
        assert_eq!((r.lokey, r.hikey), (48, 59));
        assert_eq!((r.lovel, r.hivel), (0, 63));
        assert_eq!(r.pitch_keycenter, 53);
    }

    #[test]
    fn group_opcodes_are_inherited_by_regions() {
        // Two regions under one group share lovel/hivel; each overrides
        // the key range + sample.
        let sfz = "\
<group>
lovel=64 hivel=127
<region>
sample=soft_c.wav lokey=60 hikey=60 pitch_keycenter=60
<region>
sample=soft_e.wav lokey=64 hikey=64 pitch_keycenter=64
";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions.len(), 2);
        for r in &spec.regions {
            assert_eq!(
                (r.lovel, r.hivel),
                (64, 127),
                "group velocity not inherited"
            );
        }
        assert_eq!(spec.regions[0].pitch_keycenter, 60);
        assert_eq!(spec.regions[1].pitch_keycenter, 64);
    }

    #[test]
    fn a_new_group_resets_inherited_opcodes() {
        let sfz = "\
<group>
lovel=0 hivel=63
<region>
sample=soft.wav
<group>
lovel=64 hivel=127
<region>
sample=loud.wav
";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions[0].hivel, 63);
        assert_eq!(spec.regions[1].lovel, 64);
        // The second group did not carry the first's hivel — it reset to
        // the default before applying its own opcodes.
        assert_eq!(spec.regions[1].hivel, 127);
    }

    #[test]
    fn control_default_path_prefixes_samples() {
        let sfz = "\
<control>
default_path=samples/grand/
<region>
sample=c4.wav
";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions[0].sample, "samples/grand/c4.wav");
    }

    #[test]
    fn default_path_without_trailing_slash_still_joins_once() {
        let sfz = "<control>\ndefault_path=wav\n<region>\nsample=a.wav\n";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions[0].sample, "wav/a.wav");
    }

    #[test]
    fn sample_filename_may_contain_spaces() {
        let spec = parse_sfz("<region>\nsample=grand piano c4.wav\n").expect("parses");
        assert_eq!(spec.regions[0].sample, "grand piano c4.wav");
    }

    #[test]
    fn unknown_opcodes_including_loop_points_are_ignored() {
        // The loop opcodes are deliberately outside this subset (#1682);
        // they must warn-and-ignore like any other unknown opcode rather
        // than fail the parse.
        let sfz = "\
<region>
sample=organ.wav
loop_mode=loop_continuous
loop_start=128
loop_end=4096
ampeg_attack=0.5
offset=0
";
        let spec = parse_sfz(sfz).expect("parses past unknown opcodes");
        assert_eq!(spec.regions.len(), 1);
        assert_eq!(spec.regions[0].sample, "organ.wav");
    }

    #[test]
    fn line_comments_are_stripped() {
        let sfz = "\
// a piano bank
<region>
sample=a.wav lokey=60 hikey=60 // middle C only
";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions[0].sample, "a.wav");
        assert_eq!((spec.regions[0].lokey, spec.regions[0].hikey), (60, 60));
    }

    #[test]
    fn region_without_sample_is_dropped() {
        // The first region has no sample (not playable) and is skipped;
        // the second is kept.
        let sfz = "<region>\nlokey=0 hikey=10\n<region>\nsample=b.wav\n";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions.len(), 1);
        assert_eq!(spec.regions[0].sample, "b.wav");
    }

    #[test]
    fn no_regions_is_an_error() {
        assert_eq!(
            parse_sfz("<control>\ndefault_path=x/\n"),
            Err(SfzError::NoRegions)
        );
        assert_eq!(parse_sfz(""), Err(SfzError::NoRegions));
    }

    #[test]
    fn malformed_header_is_an_error() {
        let err = parse_sfz("<region\nsample=a.wav\n").expect_err("unclosed header fails");
        assert!(matches!(err, SfzError::MalformedHeader(_)));
    }

    #[test]
    fn headers_and_opcodes_may_share_a_line() {
        let sfz = "<region> sample=a.wav lokey=60 hikey=60\n";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(spec.regions.len(), 1);
        assert_eq!(spec.regions[0].sample, "a.wav");
        assert_eq!(spec.regions[0].lokey, 60);
    }

    #[test]
    fn sample_paths_deduplicates_in_first_seen_order() {
        let sfz = "\
<region>
sample=a.wav lovel=0 hivel=63
<region>
sample=a.wav lovel=64 hivel=127
<region>
sample=b.wav
";
        let spec = parse_sfz(sfz).expect("parses");
        assert_eq!(
            spec.sample_paths(),
            vec!["a.wav".to_owned(), "b.wav".to_owned()]
        );
    }
}
