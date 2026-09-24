//! [`SecretError`]: every refusal the secrets vocabulary raises. Each names the
//! key, secret name, path, and rule it knows; none ever carries a secret value.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::PathBuf;

use super::name::SecretName;

/// A refused secret name, binding, or secret file (ADR-0235 §2, §3). The
/// `Display` names the key, the secret name, the path, and the rule, never the
/// content — and never a refused name or binding token either, since a
/// mistyped binding can hold a pasted value.
#[derive(Debug)]
pub enum SecretError {
    /// A secret name outside `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
    InvalidName {
        /// The rule the name broke.
        rule: &'static str,
    },
    /// One `<key>=<secret-name>` binding of a secret-refs knob did not parse.
    InvalidBinding {
        /// The binding's 1-based position in the comma list.
        position: usize,
        /// The rule the binding broke.
        rule: &'static str,
    },
    /// Secrets are bound but no `--secrets-dir` was given.
    NoSecretsDir {
        /// The binding's key.
        key: String,
        /// The secret the key names.
        name: SecretName,
    },
    /// A secret file broke a loader rule (size, type, mode, or value).
    Refused {
        /// The binding's key, when a binding asked for the file.
        key: Option<String>,
        /// The secret's name.
        name: SecretName,
        /// The file's path.
        path: PathBuf,
        /// The rule the file broke.
        rule: &'static str,
    },
    /// A secret file could not be read.
    Unreadable {
        /// The binding's key, when a binding asked for the file.
        key: Option<String>,
        /// The secret's name.
        name: SecretName,
        /// The file's path.
        path: PathBuf,
        /// The underlying I/O error, which carries no file content.
        source: io::Error,
    },
}

impl SecretError {
    /// Attach the binding key that asked for the file.
    #[must_use]
    pub(super) fn for_key(self, binding_key: &str) -> Self {
        match self {
            Self::Refused { name, path, rule, .. } => {
                Self::Refused { key: Some(binding_key.to_owned()), name, path, rule }
            }
            Self::Unreadable { name, path, source, .. } => {
                Self::Unreadable { key: Some(binding_key.to_owned()), name, path, source }
            }
            other => other,
        }
    }

    /// The broken rule alone, for the `--print-config` status column.
    #[must_use]
    pub(super) fn rule(&self) -> String {
        match self {
            Self::InvalidName { rule } | Self::InvalidBinding { rule, .. } | Self::Refused { rule, .. } => {
                (*rule).to_owned()
            }
            Self::NoSecretsDir { .. } => "no secrets directory".to_owned(),
            Self::Unreadable { source, .. } => format!("cannot read: {source}"),
        }
    }
}

/// `for key "k", ` when a key is known, else nothing.
fn key_clause(key: Option<&String>) -> String {
    key.map_or_else(String::new, |key| format!("for key {key:?}, "))
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { rule } => write!(f, "invalid secret name: {rule}"),
            Self::InvalidBinding { position, rule } => {
                write!(f, "secret binding {position} (`<key>=<secret-name>`) is invalid: {rule}")
            }
            Self::NoSecretsDir { key, name } => write!(
                f,
                "key {key:?} names secret `{name}`, but no secrets directory was given \
                 (pass --secrets-dir <path>; ADR-0235)"
            ),
            Self::Refused { key, name, path, rule } => {
                let key = key_clause(key.as_ref());
                write!(f, "{key}secret `{name}` at {} refused: {rule}", path.display())
            }
            Self::Unreadable { key, name, path, source } => {
                let key = key_clause(key.as_ref());
                write!(f, "{key}secret `{name}` at {} cannot be read: {source}", path.display())
            }
        }
    }
}

impl StdError for SecretError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}
