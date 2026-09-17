//! Portable reactor preparation and signature-based evaluation.
//!
//! [`Owner`] retains pushed entries and lazily constructed views. Each concrete
//! view folds once, catches up from its trusted cursor, and stays poisoned
//! after a failed fold. [`prepare()`] is a one-shot over a fresh owner.
//!
//! Authors write `#[reactor] impl Reactor for Name` with `const NAME` and
//! `#[rule]` methods. The first parameter after `&self` is a typed stored-event
//! trigger; later parameters are direct published views or named [`Guard`]s.
//! Rust infers each parameter's role from the [`Arg`] impls — the macro emits
//! `Arg<_, T, Rest>` and does not classify type names. A refutable trigger
//! pattern or [`Guard::resolve`] returning [`None`] declines that arm; it is
//! not suspended work. Each invoked arm returns exactly one [`Output`].
//!
//! [`Reactor::evaluate`] is the generated preparation/evaluation boundary.
//! [`Reactor::visit_arms`] exposes trigger, [`Params`], and output types.
//! [`reactor_bundle`] generates one views owner and one peer actor per
//! reactor: the owner folds shared views once per cluster, mails an owned
//! prepared prefix to each peer, and each peer resolves guards locally and
//! sends typed outputs to a configured external mailbox. Evaluation encodes
//! outputs through the mail codec and does not execute or append them.
//!
//! Authors keep ordinary function signatures. Generated `Arg<_, T, Rest>`
//! lists, view visitors, actor wrappers, and mail encoding are implementation
//! details. Authored view and guard types become one views-owner fold per
//! loaded cluster; peers receive [`PreparedPrefix`] mail carrying every
//! inferred [`BundledView`] snapshot and resolve guards against that prefix:
//!
//! ```ignore
//! reactor_bundle! {
//!     default = SourcePublicationViews,
//!     namespace = "test.bloomery.reactor",
//!     SourcePublisher,
//!     SourceWitness,
//! }
//! ```
//!
//! Load `SourcePublicationViews` explicitly (`export: Some(NAMESPACE)` or an
//! `export!(default = SourcePublicationViews, …)` list). Exporting the peer
//! types does not instantiate them; the views owner spawns them in `wire`.
//!
//! ```text
//! #[reactor]
//! impl Reactor for SourcePublisher {
//!     const NAME: &'static str = "source.publisher";
//!
//!     #[rule]
//!     fn publish(
//!         &self,
//!         change: HeadMoved<Tree>,
//!         current: CurrentCompilation,
//!         heads: Heads,
//!     ) -> PublicationProposal { /* ... */ }
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
extern crate self as aether_bloomery_reactor;

mod bundle;
mod direct;
mod error;
mod evaluate;
mod guard;
mod owner;
mod params;
mod prepare;
mod trigger;
mod views;

pub use aether_bloomery_reactor_derive::{reactor, reactor_bundle, rule};
pub use bundle::{
    ClusterConfig, ClusterStatus, ClusterStatusQuery, JournalEntry, PreparedPrefix, PublishedView, PushEntries,
    PushResult, extend_snapshots, snapshot_reactor, warm_reactor,
};
pub use direct::Direct;
pub use error::PrepareError;
pub use evaluate::{ArmVisitor, Intent, Output, Reactor};
pub use guard::Guard;
pub use owner::Owner;
pub use params::{Arg, AsGuard, AsView, GuardArg, Nil, Params, ViewArg};
pub use prepare::{Prepared, prepare};
pub use trigger::Trigger;
pub use views::{And, BundledView, NoViews, PublishSet, ViewSet};

#[doc(hidden)]
pub use evaluate::__macro_internals;
#[doc(hidden)]
pub use views::ViewCtor;
