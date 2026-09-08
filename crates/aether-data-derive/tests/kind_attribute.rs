//! `#[aether_data::kind(...)]` compiled against the real derives
//! (issue #5729).
//!
//! The option set -> derive list mapping is unit-tested inside the crate
//! (`src/kind_attr.rs`); what only a compile can prove is that the
//! emitted paths resolve and that the item survives expansion intact.
//! Every option appears below, so a path that stops resolving
//! (`::serde::Serialize`, `::bytemuck::Pod`) fails this target.
//!
//! The one assertion is the wire-shape tripwire. A `pod` kind is
//! cast-encoded (ADR-0005) only while its `#[repr(C)]` reaches the
//! `Kind` derive; the attribute re-emits the item verbatim precisely so
//! it does. Reprinting the item from the parsed `DeriveInput` instead —
//! or dropping the attribute — would silently demote these kinds to the
//! structured wire path, which changes bytes on the wire without
//! changing a single type signature.

use aether_data::CastEligible;

#[aether_data::kind(name = "test.attr.base")]
pub struct Base {
    pub body: String,
}

#[aether_data::kind(name = "test.attr.eq", eq)]
pub struct Compared {
    pub code: u32,
}

#[aether_data::kind(name = "test.attr.partial_eq", partial_eq)]
pub struct Measured {
    pub seconds: f32,
}

#[aether_data::kind(name = "test.attr.copy_default", copy, default)]
pub struct Reset {
    pub count: u32,
}

#[repr(C)]
#[aether_data::kind(name = "test.attr.pod", pod, default, eq)]
pub struct Vertex {
    pub x: u32,
    pub y: u32,
}

#[aether_data::kind(name = "test.attr.no_serde", no_serde)]
pub struct Bare {
    pub body: String,
}

#[aether_data::kind(name = "test.attr.extra", eq, derive(Hash, PartialOrd, Ord))]
pub struct Ordered {
    pub code: u32,
}

/// An enum kind takes the same attribute; the `Schema` derive walks the
/// variants either way.
#[aether_data::kind(name = "test.attr.outcome", eq)]
pub enum Outcome {
    Accepted,
    Rejected { reason: String },
}

/// Tripwire: `pod` must keep the cast wire shape, and the base stack
/// must keep the structured one. Both are `const` asserts because the
/// derive computes `ELIGIBLE` at compile time.
#[test]
fn pod_stays_cast_encoded_and_the_base_stack_does_not() {
    const { assert!(<Vertex as CastEligible>::ELIGIBLE) };
    const { assert!(!<Base as CastEligible>::ELIGIBLE) };
}

/// Each option exercised through the trait it stands for, so a flag
/// that stops emitting its derive is a compile error here rather than a
/// silent absence at 600 declaration sites.
#[test]
fn every_option_emits_the_trait_it_names() {
    let compared = Compared { code: 1 };
    assert_eq!(compared, Compared { code: 1 });

    let measured = Measured { seconds: 0.5 };
    assert_eq!(measured, Measured { seconds: 0.5 });

    let rejected = Outcome::Rejected { reason: "no".into() };
    assert_eq!(rejected, Outcome::Rejected { reason: "no".into() });

    assert!(Ordered { code: 1 } < Ordered { code: 2 });
    assert_eq!(Reset::default().count, 0);
    assert_eq!(Vertex::default(), Vertex { x: 0, y: 0 });

    let copyable = Reset { count: 2 };
    let moved = copyable;
    assert_eq!(copyable.count, moved.count);

    let serdeless = Bare { body: "x".into() };
    let serdeless_copy = serdeless.clone();
    assert_eq!(format!("{serdeless:?}"), format!("{serdeless_copy:?}"));

    let standard = Base { body: "y".into() };
    let standard_copy = standard.clone();
    assert_eq!(format!("{standard:?}"), format!("{standard_copy:?}"));
}
