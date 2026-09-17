//! Portable reactor preparation and signature-based evaluation.
//!
//! [`Owner`] retains pushed entries and lazily constructed views. Each concrete
//! view folds once, catches up from its trusted cursor, and stays poisoned
//! after a failed fold. [`prepare()`] is a one-shot over a fresh owner.
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
//! `aether_actor::export!(…, generators = [bundle_reactors])` collects
//! framework-owned `actors` envelopes and an `exports` selection, selects the
//! bloomery reactor extension on exported paths, keeps ordinary actors in the
//! export list, and generates one views coordinator ([`CLUSTER_NAMESPACE`])
//! plus inline reactor peers. Reactor envelopes stay on their original types.
//! The coordinator binds a caller-supplied stream token, folds shared views
//! once per cluster, mails an owned prepared prefix to each peer, and each
//! peer resolves guards locally and sends typed outputs to a configured
//! external mailbox. Live [`Event`] delivery folds n through n and freezes
//! that prefix before n+1 can change what n's peers receive. [`EventBatch`]
//! is fold-only warmup: every ordered entry is processed and no live arm
//! runs. Explicit [`PreparedResult`] / [`EvaluatedResult`] messages describe
//! preparation and evaluation; lifecycle settlement is not success.
//! Evaluation encodes outputs through the mail codec and does not execute
//! or append them.
//!
//! Authors keep ordinary function signatures. Generated `Arg<_, T, Rest>`
//! lists, view visitors, actor wrappers, and mail encoding are implementation
//! details. Authored view and guard types become one views-owner fold per
//! loaded cluster; peers receive [`PreparedPrefix`] mail carrying every
//! inferred [`BundledView`] snapshot and resolve guards against that prefix:
//!
//! ```ignore
//! aether_actor::export!(
//!     default = Probe,
//!     SourcePublisher,
//!     SourceWitness,
//!     ReactorOutputSink,
//!     generators = [aether_bloomery_reactor::bundle_reactors],
//! );
//! ```
//!
//! Load the coordinator with [`CLUSTER_NAMESPACE`]. Reactor peers are inline
//! children and are not module exports.
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
mod cluster;
mod direct;
mod error;
mod evaluate;
mod export;
mod guard;
mod owner;
mod params;
mod prepare;
mod trigger;
mod views;

#[doc(hidden)]
pub use aether_bloomery_reactor_derive::__reactor_export_generate;
pub use aether_bloomery_reactor_derive::{reactor, rule};
pub use bundle::{
    ClusterConfig, ClusterStatus, ClusterStatusQuery, EvaluatedResult, Event, EventBatch, JournalEntry, PeerEvaluated,
    PreparedPrefix, PreparedResult, PublishedView, extend_snapshots, snapshot_reactor, warm_reactor,
};
pub use cluster::Cluster;
pub use direct::Direct;
pub use error::PrepareError;
pub use evaluate::{ArmVisitor, Intent, Output, Reactor};
pub use export::CLUSTER_NAMESPACE;
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
