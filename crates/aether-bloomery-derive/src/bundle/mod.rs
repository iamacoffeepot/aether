//! The bloomery `bundle` export generator: one root for a bundle's programs and reactors.

mod classify;
mod expand;
mod input;

pub use expand::generate;
