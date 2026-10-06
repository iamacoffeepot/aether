//! Issue 7496: the first version of the watch family, which
//! `republish_watch_v2` republishes and `republish_watch_reshaped` is refused
//! over.
//!
//! Every actor is the crate's shared type: the `WatchLedger` that watches the
//! providers admitting themselves, the `WatchPeer` guest provider, and the
//! `WatchDesk` with its inline `WatchClerk`.

use aether_test_fixtures_republish::{WatchClerk, WatchDesk, WatchLedger, WatchPeer};

aether_actor::export!(public = [WatchLedger, WatchPeer, WatchDesk], private = [WatchClerk]);
