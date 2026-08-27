//! A reference-counted field is outside the Schema vocabulary — the derive
//! produces no codec and fails at the `Schema` bound rather than compiling
//! into serde.

use std::rc::Rc;

#[derive(aether_data::Schema)]
struct Counted {
    wrapped: Rc<u32>,
}

fn main() {}
