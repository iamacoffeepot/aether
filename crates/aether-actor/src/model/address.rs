//! Typed [`Address`] constructors and the caller-side candidate derivation (ADR-0230).
//!
//! The position and the proof have different owners. This module owns the
//! caller's half: turning an `Address<R>` into the candidate [`MailboxId`]
//! the address names, through `R`'s [`Resolve`] strategy. Whether anything
//! is `Live` there is the host's answer, and only resolving against the
//! registry turns the candidate into a reference.

use aether_data::{Address, AddressForm, LoadName, MailboxId};

use super::{Addressable, CallerAddressable, CallerScoped, ChildOf, Embedded, Instanced, Resolve, Singleton};
use crate::reference::ActorRef;

/// The address of the singleton `R`: resolved relative to the scope `R`'s
/// resolver selects from the caller, with no discriminator.
#[must_use]
pub fn address<R: Singleton>() -> Address<R> {
    Address::scoped(None)
}

/// The address of the embedded `R` its host loaded under `name`: a carried
/// load name folds in the namespace slot of the same fold [`address`] performs.
#[must_use]
pub fn address_named<R: Addressable<Resolver = Embedded>>(name: LoadName) -> Address<R> {
    Address::scoped(Some(name))
}

/// The address of the `R` instance keyed by `key`: resolved relative to the
/// scope `R`'s resolver selects from the caller.
#[must_use]
pub fn address_at<R: Instanced>(key: LoadName) -> Address<R> {
    Address::scoped(Some(key))
}

/// The address of the `C` instance keyed by `key` directly beneath `parent`.
/// The parent travels as a proven [`ActorRef`], so a child address can only
/// be built beneath an actor that reached `Live`.
#[must_use]
pub fn child_address<P: Addressable, C: ChildOf<P> + Instanced>(parent: ActorRef<P>, key: LoadName) -> Address<C> {
    Address::beneath(parent.id(), Some(key))
}

/// Fold `address` to the candidate position it names: `Exact` is its id,
/// `Beneath` folds from the carried parent, and `Scoped` folds from the
/// seed `R`'s resolver selects from the caller. The folding forms delegate
/// to [`Resolve::candidate`], so there is still one derivation of a
/// position. A key of the wrong shape for the strategy is `None`.
#[must_use]
pub fn address_candidate<R: CallerAddressable>(
    address: &Address<R>,
    current: MailboxId,
    parent: MailboxId,
) -> Option<MailboxId> {
    match address.form() {
        AddressForm::Exact { id } => Some(*id),
        AddressForm::Beneath { parent: beneath, key } => <<R as Addressable>::Resolver as Resolve>::candidate(
            beneath.0,
            R::NAMESPACE,
            key.as_ref().map(LoadName::as_str),
        ),
        AddressForm::Scoped { key } => {
            let seed = <<R as Addressable>::Resolver as CallerScoped>::SCOPE.select(current, parent);
            <<R as Addressable>::Resolver as Resolve>::candidate(
                seed.0,
                R::NAMESPACE,
                key.as_ref().map(LoadName::as_str),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_data::MailboxId;

    use super::super::{Embedded, Many, One};
    use super::*;

    struct RootCap;

    impl Addressable for RootCap {
        const NAMESPACE: &'static str = "test.address.root";
        type Resolver = One;
    }

    struct KeyedChild;

    impl Addressable for KeyedChild {
        const NAMESPACE: &'static str = "test.address.child";
        type Resolver = Many;
    }

    struct EmbeddedPeer;

    impl Addressable for EmbeddedPeer {
        const NAMESPACE: &'static str = "test.address.peer";
        type Resolver = Embedded;
    }

    fn load_name(text: &str) -> LoadName {
        LoadName::new(text).expect("test load names are valid segments")
    }

    #[test]
    fn exact_addresses_name_their_id() {
        let id = MailboxId(0x4010);

        assert_eq!(
            address_candidate::<RootCap>(&Address::exact(id), MailboxId(0x4020), MailboxId(0x4030)),
            Some(id),
            "an exact address is its id regardless of caller scope",
        );
    }

    #[test]
    fn scoped_addresses_fold_from_the_resolver_scope() {
        let current = MailboxId(0x4010);
        let parent = MailboxId(0x4020);

        assert_eq!(
            address_candidate::<RootCap>(&address::<RootCap>(), current, parent),
            Some(RootCap::resolve(MailboxId::NONE.0, ())),
            "a root-pinned address ignores the caller",
        );
        assert_eq!(
            address_candidate::<KeyedChild>(&address_at::<KeyedChild>(load_name("one")), current, parent),
            Some(KeyedChild::resolve(current.0, "one")),
            "a keyed address folds the carried key under the caller",
        );
        assert_eq!(
            address_candidate::<EmbeddedPeer>(&address::<EmbeddedPeer>(), current, parent),
            Some(EmbeddedPeer::resolve(parent.0, ())),
            "an embedded address folds the type namespace under the runtime parent",
        );
        assert_eq!(
            address_candidate::<EmbeddedPeer>(
                &address_named::<EmbeddedPeer>(load_name("test.address.peer-1")),
                current,
                parent,
            ),
            Some(Embedded::resolve(parent.0, "test.address.peer-1", ())),
            "a named embedded address folds the carried name under the runtime parent",
        );
    }

    #[test]
    fn beneath_addresses_fold_from_the_carried_parent() {
        let carried = MailboxId(0x4040);
        let caller = MailboxId(0x4010);
        let caller_parent = MailboxId(0x4020);

        assert_eq!(
            address_candidate::<KeyedChild>(&Address::beneath(carried, Some(load_name("one"))), caller, caller_parent,),
            Some(KeyedChild::resolve(carried.0, "one")),
            "a beneath address folds from the parent it carries, not the caller's",
        );
    }

    #[test]
    fn mismatched_address_keys_are_none() {
        let current = MailboxId(0x4010);
        let parent = MailboxId(0x4020);

        assert_eq!(
            address_candidate::<KeyedChild>(&Address::scoped(None), current, parent),
            None,
            "a keyed type with no key names no position",
        );
        assert_eq!(
            address_candidate::<RootCap>(&Address::scoped(Some(load_name("one"))), current, parent),
            None,
            "a keyless type with a key names no position",
        );
    }
}
