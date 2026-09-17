//! Portable reactor preparation: typed stored-event triggers, named guards,
//! inferred view dependencies, and owned prepared data.
//!
//! [`Owner`] retains pushed entries and lazily constructed views. Each concrete
//! view folds once, catches up from its trusted cursor, and stays poisoned
//! after a failed fold. [`prepare`] is a one-shot over a fresh owner.
//!
//! A later signature macro classifies `heads: Heads` and
//! `current: CurrentCompilation` into [`ViewArg`] / [`GuardArg`] chains. Role
//! markers keep those impls from overlapping. [`Guard::resolve`] returning
//! [`None`] declines invocation; it is not suspended work.
//!
//! ```
//! use aether_bloomery_kinds::{Digest, Entry, Head, HeadMoved, Program, Ref, Seq};
//! use aether_bloomery_reactor::{Owner, ViewArg};
//! use aether_bloomery_view::Heads;
//! use aether_data::{Kind, Storage, StorageData};
//!
//! let to = Ref::<Program>::from_digest(Digest::from_bytes([1; 32]));
//! let event = Head::<Program>::new("main").move_to(to);
//! let entry = Entry {
//!     seq: Seq(1),
//!     kind: HeadMoved::<Program>::NAME.to_owned(),
//!     cause: None,
//!     recorded_at_millis: 0,
//!     bytes: HeadMoved::<Program>::encode_storage(&StorageData::from_value(event)).unwrap(),
//! };
//! let mut owner = Owner::new();
//! owner.push(&[entry]).unwrap();
//! let (trigger, (heads, ())) =
//!     owner.prepare::<HeadMoved<Program>, ViewArg<Heads>>().unwrap().unwrap();
//! assert_eq!(trigger.head().as_str(), "main");
//! assert_eq!(heads.cursor(), Seq(1));
//! assert_eq!(heads.get(&Head::<Program>::new("main")), Some(to));
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod direct;
mod error;
mod guard;
mod owner;
mod params;
mod prepare;
mod trigger;
mod views;

pub use direct::Direct;
pub use error::PrepareError;
pub use guard::Guard;
pub use owner::Owner;
pub use params::{Arg, AsGuard, AsView, GuardArg, Nil, Params, ViewArg};
pub use prepare::{Prepared, prepare};
pub use trigger::Trigger;
pub use views::{And, NoViews, ViewSet};

#[doc(hidden)]
pub use views::ViewCtor;
