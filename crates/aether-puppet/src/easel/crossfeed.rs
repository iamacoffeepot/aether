//! Cross-feed diagnostic: the CPU wash driven by externally baked planes.
//!
//! The reference board carries its own baked region map; feeding those
//! exact planes through [`field::Sheet`] separates "our placement math
//! diverged from the board" from "our baked planes differ from the
//! board's". Ignored by default — it reads planes from a directory named
//! at run time and writes raw densities back for offline comparison, so
//! it is an instrument, not a gate.
//!
//! Drive it with:
//!
//! ```text
//! AETHER_CROSSFEED_DIR=/path/to/planes \
//!     cargo test -p aether-puppet crossfeed -- --ignored --nocapture
//! ```
//!
//! The directory holds `dims.txt` (`width height`), `label.bin` (u8 per
//! pixel), and `tone.bin` / `facing.bin` (little-endian f32 per pixel).
//! Two runs come back per material class: `density-real-N.bin` under the
//! planes as given, and `density-flat-N.bin` with facing forced to one so
//! any facing-gated coverage reduces to the bare class mask — the
//! placement the reference board applies.

use std::fs;
use std::path::Path;

use super::field::{Planes, Sheet};

/// Seed for the diagnostic sheet. Arbitrary but fixed, so two runs over
/// the same planes differ only in the planes.
const SEED: u64 = 0x5e_ed;

fn read_f32_plane(path: &Path, count: usize) -> Vec<f32> {
    let bytes = fs::read(path).expect("read f32 plane");
    assert_eq!(bytes.len(), count * 4, "{} holds one f32 per pixel", path.display());

    bytes.chunks_exact(4).map(|at| f32::from_le_bytes([at[0], at[1], at[2], at[3]])).collect()
}

fn write_f32_plane(path: &Path, plane: &[f32]) {
    fs::write(path, plane.iter().flat_map(|at| at.to_le_bytes()).collect::<Vec<u8>>()).expect("write density");
}

#[test]
#[ignore = "diagnostic instrument; needs externally baked planes"]
fn crossfeed_the_wash_with_external_planes() {
    // Instrument input, not cap config: the test is opt-in and the
    // directory only exists on the workstation driving it.
    #[allow(clippy::disallowed_methods)]
    let Ok(dir) = std::env::var("AETHER_CROSSFEED_DIR") else {
        eprintln!("AETHER_CROSSFEED_DIR unset; nothing to cross-feed");
        return;
    };
    let dir = Path::new(&dir);

    let dims = fs::read_to_string(dir.join("dims.txt")).expect("read dims.txt");
    let mut split = dims.split_whitespace().map(|at| at.parse::<usize>().expect("dimension"));
    let (width, height) = (split.next().expect("width"), split.next().expect("height"));
    let count = width * height;

    let classes = fs::read(dir.join("label.bin")).expect("read label plane");
    assert_eq!(classes.len(), count, "label.bin holds one u8 per pixel");
    let tone = read_f32_plane(&dir.join("tone.bin"), count);
    let facing = read_f32_plane(&dir.join("facing.bin"), count);
    let flat = vec![1.0; count];

    for (run, facing) in [("real", &facing), ("flat", &flat)] {
        let planes = Planes { classes: &classes, tone: &tone, facing, width, height };
        for coat in Sheet::new(planes, SEED).coats(None, None) {
            write_f32_plane(&dir.join(format!("density-{run}-{}.bin", coat.class)), &coat.density);
        }
        eprintln!("{run}: densities written to {}", dir.display());
    }
}
