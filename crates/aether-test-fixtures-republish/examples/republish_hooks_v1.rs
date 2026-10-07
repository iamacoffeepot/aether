//! Issue 7535: the first version of the hooks pair, whose replace hooks do
//! what the parent's config says (ADR-0249 §1, §2). A test republishes it with
//! a successor build of itself, or with `republish_hooks_v2`.
//!
//! Both actors are the crate's shared types, so a test can name them:
//!
//! - `HooksV1Parent` (`test.republish.hooks.parent`, root) counts each `Bump`,
//!   holds a `HeldRequest`'s reply, and spawns one `HooksV1Counter` in `wire`.
//!   Its replace hooks follow its `HookFaultConfig`.
//! - `HooksV1Counter` (`test.republish.hooks.counter`, an inline child)
//!   declares `type State`, so every dehydrate packs a child entry the
//!   successor has to rebuild.

use aether_test_fixtures_republish::{HooksV1Counter, HooksV1Parent};

aether_actor::export!(public = [HooksV1Parent], private = [HooksV1Counter]);
