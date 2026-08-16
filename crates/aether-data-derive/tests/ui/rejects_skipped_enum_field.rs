//! `skip_serializing_if` on a Schema enum-variant field is rejected —
//! positional Aether wire must encode `None`/empty values explicitly.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, aether_data::Schema)]
enum SkippedEnum {
    Ok { value: u32 },
    Err {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

fn main() {}
