//! The `PassStage::DrawSets` stage (ADR-0246 decision 4): one render
//! pass that draws the draw sets a dispatch lists for it. One file per
//! phase of the stage's life:
//!
//! - [`validate`] runs once at register: the pass's two layouts, its
//!   vertex stage's interface against them, its depth declaration, and
//!   the list slots the program's passes name between them.
//! - [`dispatch`] runs per dispatch, before anything is recorded: the
//!   lists a dispatch supplies are the number the program declared, every
//!   listed id is a live set, and each set has the layouts of the passes
//!   that draw it. Then it realizes the buffers the listed sets hold.
//! - [`encode`] runs per pass iteration, inside the open render pass: it
//!   walks the listed sets and issues their draws. Nothing is checked
//!   there, because a draw was checked when its set was made
//!   (`runtime::draw_set::check`).

pub(super) mod dispatch;
pub(super) mod encode;
pub(super) mod validate;
