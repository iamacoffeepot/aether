//! Engine secrets (ADR-0235): named files in one secrets directory, bound by
//! name in a consumer's config, held only by native capabilities.
//!
//! - A secret is one file in the directory `--secrets-dir <path>` names: the
//!   file name is the secret's [`SecretName`] and the file bytes are its value
//!   — the systemd credentials, Docker secrets, and Kubernetes secret-volume
//!   layout. [`SecretsDir`] locates the directory and applies the loader rules.
//! - Config names a secret, never its value. A consumer's knob is a
//!   [`SecretRefs`] (`<key>=<secret-name>[,…]`) carrying the derive's
//!   `#[config(secrets)]` hint, which binds it to the source stack's directory.
//! - The consumer loads its own names once, in `init`, into [`Secrets`], and
//!   moves each value into its own table as a [`Secret`]: redacted `Debug`, read
//!   only through `expose()`, not clonable or serializable, wiped on drop.
//! - Every refusal is a [`SecretError`] naming the key, name, path, and rule,
//!   never the content.
//!
//! No actor, kind, mail, init-config, journal record, argv, env var, log line,
//! or config dump carries a secret value.

mod dir;
mod error;
mod loaded;
mod name;
mod refs;
mod secret;

pub use dir::SecretsDir;
pub use error::SecretError;
pub use loaded::Secrets;
pub use name::SecretName;
pub use refs::{SecretRefs, parse_secret_refs};
pub use secret::Secret;
