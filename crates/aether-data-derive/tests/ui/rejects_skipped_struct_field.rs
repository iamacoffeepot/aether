//! `skip_serializing_if` on a Schema struct field is rejected — positional
//! Aether wire must encode `None`/empty values explicitly.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, aether_data::Schema)]
struct Skipped {
    present: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing: Option<u32>,
}

fn main() {}
