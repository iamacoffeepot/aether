//! Reactor-bundle lifecycle records the driver writes: bringing an instance
//! live for a head, its rejection, and a reaction that failed outright.
//!
//! There is no retirement record. Retirement is derived from the
//! [`crate::ReactorSet`] and head folds (ADR-0226 decision 8).

mod activation;
mod failure;

pub use activation::{Activated, ActivationRejected, LiveFromError};
pub use failure::ReactionFailed;
