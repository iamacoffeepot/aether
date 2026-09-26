//! `source.select`: the checkout inside an imported source image
//! (ADR-0237 decision 3).
//!
//! `scripts/bloomery/source/` packs a checkout into an image under `/source`,
//! and `aether.workspace.import` exports that image's whole filesystem, so the
//! imported root holds `source` beside the placeholders Docker adds to every
//! container (`.dockerenv`, `dev`, `etc`, `proc`, `sys`). The program returns
//! the `source` entry's tree as its result, so a caller cites the transition's
//! result as `proof.clippy.input.source`, and the transition records which
//! image the tree came from.
//!
//! The entry name the program selects and the recipe's `COPY . /source` are
//! one contract: rename both or neither.

mod input;

use aether_bloomery_kinds::{Detail, Mode, Name, Node, Refusal, Tree};
use aether_bloomery_program::{Env, Program, Sync, program};

pub use input::SelectInput;

/// The image root entry the source recipe copies the checkout into.
const SOURCE: &str = "source";

/// Selects the checkout from an imported source image.
///
/// Pure: the result is a subtree of the cited image, so its digest is that
/// subtree's digest, and nothing new is built.
pub struct SourceSelect;

#[program]
impl Program for SourceSelect {
    const NAME: &'static str = "source.select";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Select the checkout from an imported source image.";
    type Input = SelectInput;
    type Result = Tree;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let source = Name::new(SOURCE).map_err(|error| refused(format!("entry name {SOURCE:?}: {error}")))?;
        match env.injected(input.image)?.entries().get(&source) {
            Some(Node::Directory(tree)) => env.injected(*tree),
            Some(_) => Err(refused(format!("image tree: {SOURCE} is not a directory"))),
            None => Err(refused(format!("image tree: {SOURCE} is absent"))),
        }
    }
}

/// A refusal carrying `reason`, which names the path and the rule.
fn refused(reason: impl AsRef<str>) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}
