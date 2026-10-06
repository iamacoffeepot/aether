//! What the operating system reports about this process.

use std::fs;

/// Resident memory in mebibytes, from `/proc/self/statm` (4 KiB pages).
pub fn resident_mebibytes() -> f64 {
    let pages = fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|text| text.split_whitespace().nth(1).and_then(|field| field.parse::<f64>().ok()))
        .unwrap_or(0.0);
    pages * 4096.0 / (1024.0 * 1024.0)
}

/// User plus system CPU seconds of every thread, from `/proc/self/stat`
/// (clock ticks of 10 ms).
pub fn cpu_secs() -> f64 {
    let text = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after_name = text.rsplit_once(") ").map_or("", |(_, rest)| rest);
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    let ticks = |index: usize| fields.get(index).and_then(|field| field.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}
