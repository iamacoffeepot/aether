//! Runtime tests, co-located with the decisions they exercise.
//!
//! Every test here names a branch this crate owns: an admission verdict, a
//! registry claim, a store ceiling, or the inline-versus-addressed choice.
//! Nothing here re-runs the schema walks, the envelope parser, or the wire
//! codec — those are other modules' and other crates' tests, and repeating them
//! would only make this suite slower to run and harder to trust.

mod admission;
mod derive;
mod projection;
mod registry;
mod response_resources;
mod transport;
