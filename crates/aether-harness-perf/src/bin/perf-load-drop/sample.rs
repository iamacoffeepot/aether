//! One reading of the process's memory: resident set, `/proc/self/smaps`
//! summed by mapping class, glibc's `mallinfo2`, and dhat's live totals.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;

#[derive(Default, Clone, Copy)]
struct Class {
    mappings: u64,
    size_kib: u64,
    rss_kib: u64,
}

pub struct Sample {
    cycles: usize,
    rss_kib: u64,
    classes: BTreeMap<&'static str, Class>,
    /// `mallinfo2`: bytes in use (block headers included), free bytes the
    /// allocator holds, and bytes in mmapped chunks, summed over every arena.
    malloc_in_use: u64,
    malloc_free: u64,
    malloc_mmapped: u64,
    dhat_bytes: Option<u64>,
    dhat_blocks: Option<u64>,
}

impl Sample {
    pub fn json(&self) -> String {
        let mut out = format!(
            "{{\"cycles\":{},\"rss_kib\":{},\"malloc_in_use\":{},\"malloc_free\":{},\"malloc_mmapped\":{}",
            self.cycles, self.rss_kib, self.malloc_in_use, self.malloc_free, self.malloc_mmapped
        );
        if let (Some(bytes), Some(blocks)) = (self.dhat_bytes, self.dhat_blocks) {
            write!(out, ",\"dhat_bytes\":{bytes},\"dhat_blocks\":{blocks}").unwrap();
        }
        out.push_str(",\"smaps\":{");
        for (position, (name, class)) in self.classes.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            write!(
                out,
                "\"{name}\":{{\"mappings\":{},\"size_kib\":{},\"rss_kib\":{}}}",
                class.mappings, class.size_kib, class.rss_kib
            )
            .unwrap();
        }
        out.push_str("}}");
        out
    }
}

/// The class of one mapping, by its name and permissions.
fn class_of(perms: &str, name: &str) -> &'static str {
    match name {
        "[heap]" => "heap",
        "" if perms.starts_with("rw") => "anon_rw",
        "" if perms.contains('x') => "anon_exec",
        "" if perms.starts_with("r-") => "anon_ro",
        "" => "anon_none",
        name if name.starts_with("[stack") => "stack",
        name if name.starts_with("/memfd:") => "memfd",
        name if name.starts_with('[') => "special",
        _ => "file",
    }
}

fn kib(line: &str) -> u64 {
    line.split_whitespace().nth(1).and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn smaps() -> BTreeMap<&'static str, Class> {
    let mut classes: BTreeMap<&'static str, Class> = BTreeMap::new();
    let Ok(text) = fs::read_to_string("/proc/self/smaps") else {
        return classes;
    };
    let mut current = "";
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let first = fields.next().unwrap_or("");
        let header = first.contains('-') && !first.ends_with(':');
        if header {
            let perms = fields.next().unwrap_or("");
            let name = fields.nth(3).unwrap_or("");
            current = class_of(perms, name);
            classes.entry(current).or_default().mappings += 1;
        } else if first == "Size:" {
            classes.entry(current).or_default().size_kib += kib(line);
        } else if first == "Rss:" {
            classes.entry(current).or_default().rss_kib += kib(line);
        }
    }
    classes
}

fn status_kib(field: &str) -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| text.lines().find(|line| line.starts_with(field)).map(kib))
        .unwrap_or(0)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn mallinfo() -> (u64, u64, u64) {
    // SAFETY: `mallinfo2` takes no arguments and returns a plain struct by
    // value.
    let info = unsafe { libc::mallinfo2() };
    (info.uordblks as u64, info.fordblks as u64, info.hblkhd as u64)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn mallinfo() -> (u64, u64, u64) {
    (0, 0, 0)
}

pub fn take(cycles: usize, profiled: bool) -> Sample {
    let (malloc_in_use, malloc_free, malloc_mmapped) = mallinfo();
    let stats = profiled.then(dhat::HeapStats::get);
    Sample {
        cycles,
        rss_kib: status_kib("VmRSS:"),
        classes: smaps(),
        malloc_in_use,
        malloc_free,
        malloc_mmapped,
        dhat_bytes: stats.as_ref().map(|stats| stats.curr_bytes as u64),
        dhat_blocks: stats.as_ref().map(|stats| stats.curr_blocks as u64),
    }
}

/// A reading after `malloc_trim(0)`: free heap pages the allocator can give
/// back are returned first, so what remains is in use or pinned by a live
/// block above it.
pub fn after_trim(cycles: usize, profiled: bool) -> Sample {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only releases free memory the allocator holds.
    unsafe {
        libc::malloc_trim(0);
    }
    take(cycles, profiled)
}
