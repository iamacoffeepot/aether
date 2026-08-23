//! The mutating half: write the widened revision, sign it, store the approval.
//!
//! Every step here is check-then-act, and the signature between them is
//! deterministic, so re-running the command after a failure converges rather
//! than compounding. The one irreversible moment is the revision write — it
//! advances the commission's tip, and until an approval lands against the new
//! tip the member is unsealable — which is why the caller announces it before
//! it happens.

use std::fmt;
use std::fs;
use std::path::Path;
use std::str;

use aether_bloomery::{
    Digest, KeyId, SCOPE_VERIFY_SCHEMA, ScopeRevision, ScopeVerifyInput, Statement, digest_of, signed_approval,
    signed_cancel, signed_reopen,
};
use anyhow::{Context, Result, bail};

use crate::bloom::client::Client;
use crate::bloom::dto::{CommissionShowView, DigestHex, RevisionEvidence};

/// The operator's Approve-door signing key, loaded from a seed file.
///
/// The coordinator holds no private keys, so every approval at every tier
/// needs a signature minted here. Custody is the operator's, and the file mode
/// check is the one thing this can do about it.
pub struct OperatorKey {
    pub signer: KeyId,
    seed: [u8; 32],
}

/// Hand-written rather than derived: the seed is a private signing key, and a
/// derived `Debug` would put it in every panic message and test failure that
/// prints one.
impl fmt::Debug for OperatorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorKey").field("signer", &self.signer).field("seed", &"<redacted>").finish()
    }
}

impl OperatorKey {
    /// Load a 32-raw-byte or 64-hex seed from `path`.
    ///
    /// Refuses a file any group or other can read: a signing seed readable by
    /// another account on the host is a key that is no longer the operator's,
    /// and a tool that shrugs at that teaches the habit.
    pub fn load(signer: KeyId, path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read signing seed {}", path.display()))?;
        refuse_loose_mode(path)?;
        let seed = decode_seed(&bytes)
            .with_context(|| format!("signing seed {} is neither 32 raw bytes nor 64 hex", path.display()))?;
        Ok(Self { signer, seed })
    }

    /// The Approve-door statement over `scope`.
    ///
    /// Deterministic: the same seed over the same revision digest re-mints
    /// byte-identical bytes with a byte-identical address, which is what makes
    /// a re-run of the whole command a no-op at the store.
    pub fn approval_of(&self, scope: Digest) -> Statement {
        signed_approval(self.signer.clone(), &self.seed, scope)
    }

    /// The Cancel-door statement over `intent`.
    ///
    /// Deterministic for the same reason [`Self::approval_of`] is: a re-run re-mints
    /// the same bytes, and the store's not-open refusal is what a second
    /// attempt hits.
    pub fn cancel_of(&self, intent: Digest) -> Statement {
        signed_cancel(self.signer.clone(), &self.seed, intent)
    }

    /// The Reopen-door statement over `intent`.
    ///
    /// Deterministic for the same reason [`Self::cancel_of`] is; the store's
    /// not-landed refusal is what a second attempt hits.
    pub fn reopen_of(&self, intent: Digest) -> Statement {
        signed_reopen(self.signer.clone(), &self.seed, intent)
    }
}

fn decode_seed(bytes: &[u8]) -> Result<[u8; 32]> {
    if let Ok(raw) = <[u8; 32]>::try_from(bytes) {
        return Ok(raw);
    }
    let text = str::from_utf8(bytes).context("seed is not raw bytes and not UTF-8 hex")?;
    let text = text.trim();
    if text.len() != 64 {
        bail!("hex seed is {} characters, not 64", text.len());
    }
    let mut seed = [0_u8; 32];
    for (index, slot) in seed.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).context("seed is not hex")?;
    }
    Ok(seed)
}

#[cfg(unix)]
fn refuse_loose_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = fs::metadata(path).with_context(|| format!("stat signing seed {}", path.display()))?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!("signing seed {} is mode {:o}; make it 0600 before signing with it", path.display(), mode & 0o777);
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_loose_mode(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write `widened` as the commission's next revision, returning its address.
///
/// Idempotent by content: re-posting bytes the store already holds as the tip
/// answers with the same digest, so a re-run after a downstream failure does
/// not advance the commission a second time.
pub fn write_widened(client: &Client<'_>, workpiece: &str, widened: &ScopeRevision) -> Result<DigestHex> {
    let evidence = RevisionEvidence {
        scope_verify: Some(ScopeVerifyInput {
            schema: SCOPE_VERIFY_SCHEMA,
            named_paths: Vec::new(),
            named_symbols: Vec::new(),
            declared_surface: widened.declared_surface.clone(),
        }),
    };
    let expected = DigestHex::from_bytes(*digest_of(widened).as_bytes());
    match client.write_revision(workpiece, widened, &evidence) {
        Ok(written) => {
            if written.digest != expected {
                bail!("the coordinator stored {} for a revision addressed {expected}", written.digest);
            }
            Ok(written.digest)
        }
        Err(error) if error.to_string().contains("already stored") => Ok(expected),
        Err(error) => Err(error),
    }
}

/// Store the operator's approval of `scope`, unless one is already there.
///
/// The skip is on the commission's own approval list rather than on an error
/// string, because a duplicate submission is a normal re-run and reading it
/// off the state is cheaper and clearer than provoking a `409`.
pub fn approve(
    client: &Client<'_>,
    commission: &CommissionShowView,
    workpiece: &str,
    scope: DigestHex,
    key: &OperatorKey,
) -> Result<bool> {
    if commission.approvals.iter().any(|statement| statement.words.as_slice() == scope.as_bytes().as_slice()) {
        return Ok(false);
    }
    client.approve(workpiece, &key.approval_of(Digest::from_bytes(*scope.as_bytes())))?;
    Ok(true)
}
