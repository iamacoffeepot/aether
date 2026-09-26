//! Declared links (ADR-0230 §2): which actor types an actor may write a typed
//! path to.

use super::Addressable;

/// `Self: LinksTo<R>` means the actor declares a link to `R`, so its ctx may
/// write an [`ActorPath<R>`](crate::ActorPath) with `ctx.link::<R>(&key)` or
/// `ctx.link_child::<P, R>(&parent, &key)`. Declared with
/// `#[actor(links(R))]`, one list per actor.
///
/// A link is not a dependency. A path claims nothing and is proven again
/// where it is received, so a link needs no live target: `R` is any
/// [`Addressable`], an [`Instanced`](crate::Instanced) actor included, which
/// `depends(..)` refuses.
///
/// A safe trait. A hand-written impl is the same hole #6842 closes for
/// [`Contract<K>`](crate::Contract), and `LinksTo<R>` follows that outcome.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not declare a link to `{R}`",
    note = "add `{R}` to the `links(...)` list on the actor's `#[actor(...)]` attribute"
)]
pub trait LinksTo<R: Addressable>: Addressable {}
