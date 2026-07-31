//! CI enforcement of the properties [`super::test_targets`]' narrowing
//! rests on (issue #4215).
//!
//! Issue #4197 narrowed a test-only diff from 31 affected packages to
//! one. That is correct only while a short list of structural properties
//! holds, and each was established by reading the tree once. When one
//! stops holding, `cargo xtask affected` under-selects **silently**: a
//! package that should have run its tests does not, and nothing says so
//! until `main` goes red later with no changed-file signal to attribute
//! it to. A missed package is the one failure this tool must never have —
//! a slow suite costs minutes, a miss costs a bisect.
//!
//! So the properties are recomputed here on every push rather than
//! recorded in a comment. [`test_isolation`] covers the ones that keep a
//! `tests/` tree from feeding anything but its own test binaries;
//! [`dist_consumers`] covers the precondition on the other side of the
//! selection — whether the `cargo xtask dist` pre-build runs for the
//! packages that need it.
//!
//! Two properties from #4197's list are deliberately absent. A **nested
//! workspace member** is not a guppy workspace member, so a path under
//! its `tests/` directory never matches a member root, falls through to
//! the determinator, matches no package, and hits the built-in
//! mark-everything fallback — conservative, not a miss, so there is
//! nothing to guard. A **build script reading `tests/`** is covered as
//! far as it can be: a `include_str!`-style compile-time read is checked
//! like any other source reference, but a runtime `fs::read` inside
//! `build.rs` cannot be told from any other path string without
//! flagging every literal a build script mentions. Both build scripts in
//! the workspace only shell out to `git rev-parse` and read cargo's own
//! `PROFILE` / `TARGET`, and [`test_isolation`]'s target check keeps
//! `build.rs` itself out of the `tests/` tree.

mod dist_consumers;
mod source;
mod test_isolation;
mod workspace;
