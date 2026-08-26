//! Which policy the amendment is judged against, and the gate over the delta.
//!
//! The tier verdict itself is [`aether_bloomery`]'s
//! ([`aether_bloomery::tier_verdict`] / [`aether_bloomery::gate_widening`]) —
//! the same implementation the seal door's `resolve_surface` stands on, so this
//! command and the coordinator cannot decide differently. What lives here is the half that *reads*: which
//! `ApprovalPolicy` the successor's seal will actually gate against.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{ApprovalPolicy, Tier, TierVerdict};
use anyhow::{Context, Result, bail};

use crate::bloom::client::Client;
use crate::bloom::dto::{BloomSpec, DigestHex};

/// The kind name a sealed approval policy is registered under.
const APPROVAL_POLICY_KIND: &str = "aether.bloomery.approval_policy";

/// Where the resolved policy came from — reported so a refusal names the
/// authority it refused under rather than leaving the operator to guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicySource {
    /// The predecessor bloom's sealed `aether.bloomery.approval_policy`.
    Sealed(DigestHex),
    /// The repository policy file, because the predecessor sealed none.
    File(PathBuf),
}

impl PolicySource {
    pub fn describe(&self) -> String {
        match self {
            Self::Sealed(digest) => format!("sealed config {}", digest.as_hex()),
            Self::File(path) => format!("file {}", path.display()),
        }
    }
}

/// The policy the successor's seal will gate against.
///
/// The predecessor's sealed policy when it sealed one, read back through
/// `GET /configs/{digest}`; the repository file only when it sealed none. A
/// sealed entry that cannot be read or parsed is a **refusal**, never a
/// fallback: falling back would answer with a policy the seal will not use,
/// and a tier verdict against the wrong policy is worse than no verdict at all
/// — it reads as authoritative.
pub fn resolve_policy(client: &Client<'_>, spec: &BloomSpec, file: &Path) -> Result<(ApprovalPolicy, PolicySource)> {
    let Some(address) = spec.configs().address_of(APPROVAL_POLICY_KIND) else {
        let text = fs::read_to_string(file).with_context(|| format!("read approval policy {}", file.display()))?;
        let policy: ApprovalPolicy =
            toml::from_str(&text).with_context(|| format!("parse approval policy {}", file.display()))?;
        return Ok((policy, PolicySource::File(file.to_owned())));
    };

    let hex = address.to_hex();
    let stored = client
        .config(&hex)
        .with_context(|| format!("read the bloom's sealed approval policy {hex}; the amendment cannot be judged"))?;
    if stored.kind != APPROVAL_POLICY_KIND {
        bail!("sealed config {hex} is filed as `{}`, not an approval policy", stored.kind);
    }
    let policy: ApprovalPolicy = serde_json::from_value(stored.value)
        .with_context(|| format!("the bloom's sealed approval policy {hex} does not decode"))?;
    Ok((policy, PolicySource::Sealed(DigestHex::from(address))))
}

/// The verdict table `--dry-run` prints, and the same table a refusal carries.
pub fn render(verdict: &TierVerdict, source: &PolicySource, ceiling: Tier) -> String {
    let mut out = format!(
        "policy     {}\nceiling    {:?}\nexisting   {:?}\nwidened    {:?}\n",
        source.describe(),
        ceiling,
        verdict.existing,
        verdict.widened
    );
    for (glob, tier) in &verdict.per_added {
        let _ = writeln!(out, "  + {glob}  {tier:?}");
    }
    out
}
