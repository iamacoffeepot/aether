//! Portable reactor preparation and signature-based evaluation.
//!
//! [`Owner`] retains pushed entries and lazily constructed views. Each concrete
//! view folds once, catches up from its trusted cursor, and stays poisoned
//! after a failed fold. A [`Root`] releases entries once every view has folded
//! them and keeps the last as the trigger. [`prepare()`] is a one-shot over a
//! fresh owner.
//!
//! Authors write `#[reactor] impl Reactor for Name` with `const NAMESPACE` and
//! `#[rule]` methods. The first parameter after `&self` is a typed stored-event
//! trigger; later parameters are direct published views or named [`Guard`]s.
//! Rust infers each parameter's role from the [`Arg`] impls — the macro emits
//! `Arg<_, T, Rest>` and does not classify type names. A refutable trigger
//! pattern, a well-formed event of a different typed specialization, or
//! [`Guard::resolve`] returning [`None`] declines that arm; it is not
//! suspended work. Each invoked arm returns exactly one [`Output`].
//!
//! [`Reactor::evaluate`] is the generated preparation/evaluation boundary.
//! [`Reactor::visit_arms`] exposes trigger, [`Params`], and output types.
//! `aether_actor::export!(…, generators = [aether_bloomery_bundle::bundle])`
//! collects framework-owned `actors` envelopes and an `exports` selection,
//! selects the bloomery reactor extension on exported paths, keeps ordinary
//! actors in the export list, and generates one digest-loaded root at
//! `aether.bloomery.bundle` (`aether_bloomery_kinds::BUNDLE_NAMESPACE`)
//! wrapping [`Root`]. Reactor envelopes stay on their original types. The root
//! takes no config, owns the views, calls each reactor's `evaluate` directly,
//! and answers `Warm` / `Event` / `StatusQuery` to its caller. A request with
//! no reply target is ignored. Evaluation encodes outputs through the mail
//! codec as attributed [`aether_bloomery_kinds::ReactorIntent`] values and
//! does not execute or append them. `aether_bloomery_bundle::bundle` also pins
//! each selected reactor's const-assembled declaration record into the
//! `aether.bloomery.reactors` custom section, so native readers can decode
//! reactor names and rule kinds from artifact bytes before loading.
//!
//! Authors keep ordinary function signatures. Generated `Arg<_, T, Rest>`
//! lists, view visitors, actor wrappers, and mail encoding are implementation
//! details. Authored view and guard types become one views-owner fold per
//! loaded digest:
//!
//! ```ignore
//! aether_actor::export!(
//!     SourcePublisher,
//!     SourceWitness,
//!     generators = [aether_bloomery_bundle::bundle],
//! );
//! ```
//!
//! Load the root with `aether_bloomery_kinds::BUNDLE_NAMESPACE` under the journal artifact digest.
//!
//! ```text
//! #[reactor]
//! impl Reactor for SourcePublisher {
//!     const NAMESPACE: &'static str = "test.bloomery.source.publisher";
//!
//!     #[rule]
//!     fn publish(
//!         &self,
//!         change: HeadMoved<Tree>,
//!         current: CurrentCompilation,
//!         heads: Heads,
//!     ) -> SetHead { /* ... */ }
//! }
//! ```
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
//!     kind: HeadMoved::<Program>::ID,
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
extern crate self as aether_bloomery_reactor;
// Keep the existing serde dependency referenced so cargo-machete does not
// require a Cargo.toml edit outside this issue's declared surface.
use serde as _;

mod direct;
mod error;
mod evaluate;
mod guard;
mod owner;
mod params;
mod prepare;
mod reactors;
mod root;
mod trigger;
mod views;

pub use aether_bloomery_kinds as kinds;
pub use aether_bloomery_reactor_derive::{reactor, rule};
pub use direct::Direct;
pub use error::PrepareError;
pub use evaluate::{ArmVisitor, Intent, Output, Reactor};
pub use guard::Guard;
pub use owner::Owner;
pub use params::{Arg, AsGuard, AsView, GuardArg, Nil, Params, ViewArg};
pub use prepare::{Prepared, prepare};
pub use reactors::{EvaluateFail, ReactorList};
pub use root::Root;
pub use trigger::Trigger;
pub use views::{And, NoViews, ViewSet};

#[doc(hidden)]
pub use evaluate::__macro_internals;
#[doc(hidden)]
pub use views::ViewCtor;
