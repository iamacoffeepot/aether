//! ADR-0088 §3 link-time `NameEntry` submissions for the chassis-owned
//! mailbox namespaces.
//!
//! Each chassis cap owns a mailbox name (`aether.audio`, `aether.fs`, …)
//! declared as its `Actor::NAMESPACE` const. ADR-0088 reverses a
//! [`MailboxId`](aether_data::MailboxId) back to that name through the
//! static reverse map, which folds the [`NameEntry`] inventory at boot.
//! This module submits one `NameEntry` per chassis mailbox, keyed on the
//! cap's own `NAMESPACE` const so there is no string-literal drift — if a
//! cap's mailbox name changes, the entry follows automatically.
//!
//! Native-only: the `inventory` crate the submissions ride on does not
//! link on `wasm32-unknown-unknown`, exactly like the `Kind` descriptor
//! inventory. The header-only wasm build of this crate skips the module.
//!
//! Feature-conditional caps (`render`, `audio`) gate their submissions to
//! match the cap's own availability so a build without the feature
//! doesn't reference an absent type. Instanced families (the trampoline
//! mailbox, `aether.component.trampoline:NAME`) are covered by the
//! `Dynamic` thread-name-style runtime registration rather than a static
//! `NameEntry`, so they are not submitted here.

#![cfg(not(target_arch = "wasm32"))]

use aether_actor::Actor;
use aether_data::MAILBOX_DOMAIN;
use aether_data::name_inventory::{NameEntry, inventory};

use crate::anthropic::AnthropicCapability;
use crate::component::ComponentHostCapability;
use crate::dag::DagCapability;
use crate::fs::FsCapability;
use crate::gemini::GeminiCapability;
use crate::handle::HandleCapability;
use crate::http::HttpCapability;
use crate::input::InputCapability;
use crate::trace::TraceDispatchCapability;
use crate::window::HeadlessWindowCapability;

#[cfg(feature = "audio")]
use crate::audio::AudioCapability;
#[cfg(feature = "render")]
use crate::render::RenderCapability;

/// Submit a [`NameEntry`] for one chassis cap's mailbox name, pulling the
/// name from the cap's `Actor::NAMESPACE` const so it can't drift from
/// the authoritative declaration.
macro_rules! submit_mailbox_name {
    ($cap:ty) => {
        inventory::submit! {
            NameEntry {
                domain: MAILBOX_DOMAIN,
                name: <$cap as Actor>::NAMESPACE,
            }
        }
    };
}

submit_mailbox_name!(HandleCapability);
submit_mailbox_name!(TraceDispatchCapability);
submit_mailbox_name!(HttpCapability);
submit_mailbox_name!(FsCapability);
submit_mailbox_name!(ComponentHostCapability);
submit_mailbox_name!(InputCapability);
submit_mailbox_name!(HeadlessWindowCapability);
submit_mailbox_name!(DagCapability);
submit_mailbox_name!(AnthropicCapability);
submit_mailbox_name!(GeminiCapability);

#[cfg(feature = "render")]
submit_mailbox_name!(RenderCapability);

#[cfg(feature = "audio")]
submit_mailbox_name!(AudioCapability);
