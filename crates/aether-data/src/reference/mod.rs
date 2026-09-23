//! ADR-0230 exportable reference vocabulary: [`Namespace`], [`LoadName`],
//! [`Address`], and [`ActorPath`]. These are the forms that may cross a
//! boundary, because none of them claims that an actor exists. The proven
//! reference types are memory-only and live beside `Addressable` in
//! `aether-actor`.

mod actor_path;
mod address;
mod load_name;
mod namespace;
pub(crate) mod segment;

pub use actor_path::{ActorPath, ActorPathError, ActorPathForm, PathSegment};
pub use address::{Address, AddressForm};
pub use load_name::{LoadName, LoadNameError};
pub use namespace::Namespace;
pub use segment::SegmentFault;
