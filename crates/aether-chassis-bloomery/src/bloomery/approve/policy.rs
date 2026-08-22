//! The filesystem side of the tier policy: read `approval-policy.toml` off the
//! host and hand back the canonical [`ApprovalPolicy`] value.
//!
//! The policy itself — the `{default, rules}` table and the most-restrictive-wins
//! resolver — lives in `aether-bloomery` as the sealed
//! `aether.bloomery.approval_policy` kind (#4616), so a bloom can attest the
//! policy its members were admitted under rather than inheriting whatever text
//! was on the coordinator's disk. What stays here is the one thing that value
//! type cannot do: reach a file, and parse that file through the `toml` crate
//! into the typed policy (`deny_unknown_fields`, fail-closed).
//!
//! The file remains the **fallback** a bloom that seals no policy entry resolves
//! to, which is what keeps a coordinator that has authored none working
//! unchanged. Either failure below is a gate failure, never a silent tier.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

pub use aether_bloomery::{ApprovalPolicy, Tier};

/// Why a policy artifact could not become a usable [`ApprovalPolicy`]. Either
/// case is a **gate failure**, never a silent tier.
#[derive(Debug)]
pub enum PolicyError {
    /// The policy file could not be read.
    Unreadable(io::Error),
    /// The file was read but is not a well-formed policy (fail-closed parse).
    Malformed,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => write!(f, "policy file could not be read: {error}"),
            Self::Malformed => write!(f, "policy file is not a well-formed approval policy"),
        }
    }
}

impl Error for PolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unreadable(error) => Some(error),
            Self::Malformed => None,
        }
    }
}

/// Parse a policy from its TOML text, or `None` if it is malformed
/// (fail-closed). Unknown keys, unknown tier spellings, and structural
/// surprises are refused by the type; a rule glob outside the policy grammar
/// is refused after deserialize.
#[must_use]
pub fn parse_policy(text: &str) -> Option<ApprovalPolicy> {
    let policy: ApprovalPolicy = toml::from_str(text).ok()?;
    policy.rules_in_grammar().then_some(policy)
}

/// Read and parse the fallback tier policy from a repository path.
///
/// # Errors
/// [`PolicyError::Unreadable`] if the file cannot be read, or
/// [`PolicyError::Malformed`] if its contents are not a well-formed policy.
pub fn load_policy(path: &Path) -> Result<ApprovalPolicy, PolicyError> {
    parse_policy(&fs::read_to_string(path).map_err(PolicyError::Unreadable)?).ok_or(PolicyError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::{ApprovalPolicy, parse_policy};

    fn parse(text: &str) -> Option<ApprovalPolicy> {
        parse_policy(text)
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(
            parse("default = \"judge\"\nextra = true\n[[rules]]\nglob = \"docs/**\"\ntier = \"auto\"\n").is_none(),
            "an unknown top-level key must fail closed"
        );
        assert!(
            parse("default = \"judge\"\n[[rules]]\nglob = \"docs/**\"\ntier = \"auto\"\nnote = \"x\"\n").is_none(),
            "an unknown rule key must fail closed"
        );
    }

    #[test]
    fn unknown_tier_spellings_are_refused() {
        assert!(
            parse("default = \"owner\"\n[[rules]]\nglob = \"docs/**\"\ntier = \"auto\"\n").is_none(),
            "an unknown default tier must fail closed"
        );
        assert!(
            parse("default = \"judge\"\n[[rules]]\nglob = \"docs/**\"\ntier = \"owner\"\n").is_none(),
            "an unknown rule tier must fail closed"
        );
    }

    #[test]
    fn malformed_structure_is_refused() {
        assert!(parse("").is_none(), "empty text must fail closed");
        assert!(
            parse("[[rules]]\nglob = \"docs/**\"\ntier = \"auto\"\n").is_none(),
            "a missing default must fail closed"
        );
        assert!(parse("default = \"judge\"\n").is_none(), "a missing rules table must fail closed");
        assert!(
            parse("default = [\"judge\"]\n[[rules]]\nglob = \"docs/**\"\ntier = \"auto\"\n").is_none(),
            "a default that is not a tier must fail closed"
        );
        assert!(
            parse("default = \"judge\"\n[[rules]]\nglob = \"docs//**\"\ntier = \"auto\"\n").is_none(),
            "an out-of-grammar rule glob must fail closed"
        );
    }
}
