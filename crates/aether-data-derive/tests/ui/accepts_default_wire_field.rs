//! `#[serde(default)]` on a Schema field compiles — defaulting does not
//! omit encoded bytes.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.defaulted")]
struct Defaulted {
    present: u32,
    #[serde(default)]
    extra: Option<u32>,
}

fn main() {
    let value = Defaulted {
        present: 1,
        extra: None,
    };
    let _ = (value.present, value.extra);
}
