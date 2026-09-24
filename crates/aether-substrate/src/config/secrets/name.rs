//! [`SecretName`]: the validated name of one secret file (ADR-0235 §2).

use std::fmt;

use super::error::SecretError;

/// The longest secret name, in bytes.
const MAX_NAME_BYTES: usize = 64;

/// The name of one secret: the file name inside the secrets directory, and the
/// reference config carries in place of the value (ADR-0235 §3).
///
/// A name matches `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`. It holds no path
/// separator and cannot start with a dot, so no name reaches outside the
/// directory. Names are not secret, so `SecretName` has a `Display`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SecretName {
    type Error = SecretError;

    /// Validate `name` against the grammar. The refusal names the rule, never
    /// the refused text: a mistyped binding can hold a pasted value.
    fn try_from(name: &str) -> Result<Self, Self::Error> {
        let rule = if name.is_empty() {
            Some("a secret name is empty")
        } else if name.len() > MAX_NAME_BYTES {
            Some("a secret name is longer than 64 bytes")
        } else if !name.as_bytes()[0].is_ascii_alphanumeric() {
            Some("a secret name must start with an ASCII letter or digit")
        } else if !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
            Some("a secret name may hold only ASCII letters, digits, `.`, `_`, and `-`")
        } else {
            None
        };
        if let Some(rule) = rule {
            return Err(SecretError::InvalidName { rule });
        }
        Ok(Self(name.to_owned()))
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::SecretName;

    #[test]
    fn secret_names_cannot_leave_the_directory() {
        let too_long = "a".repeat(65);
        for refused in ["../x", "a/b", ".hidden", "", "..", "a\\b", too_long.as_str()] {
            assert!(SecretName::try_from(refused).is_err(), "{refused:?} must be refused");
        }
        let longest = "a".repeat(64);
        for accepted in ["anthropic", "muse.key", "A-1_b", longest.as_str()] {
            assert!(SecretName::try_from(accepted).is_ok(), "{accepted:?} must be accepted");
        }
    }
}
