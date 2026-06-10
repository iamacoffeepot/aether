//! Regenerate the `decode_schema` seed corpus.
//!
//! Writes one file per table schema into `fuzz/corpus/decode_schema/`:
//! the schema's selector byte followed by a valid `encode_schema`
//! frame for that schema. Run from the `fuzz/` directory with
//! `cargo run --bin gen-corpus`. The committed corpus is the source of
//! truth; this tool just reproduces it deterministically.

use std::fs;
use std::path::Path;

use aether_codec::encode_schema;
use aether_codec_fuzz::{schema_for, seeds};

fn main() {
    let dir = Path::new("corpus/decode_schema");
    fs::create_dir_all(dir).expect("create corpus dir");

    for (selector, value) in seeds() {
        let schema = schema_for(selector);
        let mut frame = encode_schema(&value, &schema)
            .unwrap_or_else(|e| panic!("encode seed for selector {selector}: {e:?}"));
        frame.insert(0, selector);

        let path = dir.join(format!("seed-{selector:02}"));
        fs::write(&path, &frame).expect("write corpus seed");
        println!("wrote {} ({} bytes)", path.display(), frame.len());
    }
}
