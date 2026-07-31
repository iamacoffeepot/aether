//! ADR-0168 §3: the declaration an effect's emission site makes about what,
//! if anything, orders it.
//!
//! [`SettlementHold`](super::trace::SettlementHold) answers "does this effect
//! hold a chain" at runtime, and since ADR-0168 §2 a hold that holds nothing
//! cannot be built. Neither answers the question a reader actually has at an
//! emission site, which is *why* — a site that takes no hold may be wrong, or
//! may be one of two right things, and the three read identically. #4199's
//! inventory nearly filed a correct site as a violation on exactly that
//! ambiguity.
//!
//! [`EffectChain`] is that answer, spelled as a value the emitting API
//! requires rather than a paragraph beside the call. ADR-0080 §12 stated the
//! same rule in prose and was violated by construction for two months without
//! anyone noticing, which is the argument for making it an argument.

use crate::mail::MailId;

/// What orders an effect emitted at this site.
///
/// Every arm is a legitimate thing to be. The point is that they are told
/// apart at the site rather than reconstructed by an auditor tracing where
/// the emitting context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an EffectChain is the site's declaration; discarding it declares nothing"]
pub enum EffectChain {
    /// The effect holds this chain, so the chain's `Settled` covers it. The
    /// ordinary case, and the one ADR-0168 §1 requires wherever a causing
    /// chain exists.
    Held(MailId),
    /// No chain caused this effect. Settlement cannot describe it and no
    /// consumer can wait for it — a property of the effect, not an omission
    /// at the site.
    Uncaused(Uncaused),
    /// A chain caused this effect and stays open across it, held that way by
    /// a device other than a settlement hold.
    OrderedBy(OrderingDevice),
}

impl EffectChain {
    /// The chain a hold taken under this declaration would gate.
    ///
    /// [`MailId::NONE`] for both chainless arms, which
    /// [`acquire_settlement_hold`](super::trace::TraceHandle::acquire_settlement_hold)
    /// answers with `None` (ADR-0168 §2) — so declaring a case cannot
    /// manufacture a hold, and the two facts cannot drift apart.
    #[must_use]
    pub fn held_root(self) -> MailId {
        match self {
            Self::Held(root) => root,
            Self::Uncaused(_) | Self::OrderedBy(_) => MailId::NONE,
        }
    }
}

/// Why no chain caused an effect.
///
/// Each variant is a position in the engine's lifecycle from which no mail is
/// in scope, so there is nothing for the effect to descend from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uncaused {
    /// A chassis-boot birth. Boot runs before any mail exists, so the rule
    /// holds over an empty chain.
    ChassisBoot,
    /// An embedder thread reaching into the chassis — `spawn_actor`,
    /// `boot_pumped_actor` — carries no mail of its own.
    EmbedderCall,
    /// The actor close tail. It runs its registry work after the closing
    /// chain has already recorded `Finished`, so no root remains to hold.
    /// ADR-0168 names this as an effect settlement cannot describe rather
    /// than one it describes wrongly; this declaration makes the absence
    /// visible without pretending to order it.
    CloseTail,
}

/// The device ordering an effect that takes no settlement hold.
///
/// A device here has to reach the same guarantee a hold does — the causing
/// chain must not settle while the effect is still unapplied — by some other
/// means. Naming which one is the whole job: the reasoning is never visible
/// from the emitting call alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderingDevice {
    /// A retained [`InboundMail`](crate::chassis::inbox::InboundMail) reply
    /// debt. The request's `Finished` stays un-recorded while the debt is
    /// held, so its chain cannot settle across the effect; answering the debt
    /// once the effect has landed is what releases it.
    RetainedReplyDebt,
}
