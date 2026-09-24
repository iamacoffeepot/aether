//! [`SecretRefs`]: the names a consumer's config knob binds (ADR-0235 §3) —
//! ordinary config that holds references, never values.

use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use super::dir::SecretsDir;
use super::error::SecretError;
use super::loaded::Secrets;
use super::name::SecretName;

/// A consumer's secret bindings: an ordered comma list of
/// `<key>=<secret-name>`, where the key follows the consumer's own grammar
/// (the http cap's is `<host>/<header-name>`). This is Kubernetes'
/// `secretKeyRef` shape: the reference is ordinary config, and the value lives
/// in the secrets directory (ADR-0235 §3). A knob of this type holds only
/// names, so its `--print-config` row is safe.
///
/// A `#[config(secrets)]` field wires the parse on the env, file, and argv
/// sides, and its `ConfigMember::resolve` [binds](Self::bind) the refs to the
/// source stack's secrets directory. The consumer then
/// [loads](Self::load) the values once, in `init`, and holds nothing it does
/// not bind. There is no `Serialize`.
#[derive(Clone, Debug, Default)]
pub struct SecretRefs {
    entries: Vec<(String, SecretName)>,
    dir: Option<SecretsDir>,
}

impl SecretRefs {
    /// The bindings, in written order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &SecretName)> {
        self.entries.iter().map(|(key, name)| (key.as_str(), name))
    }

    /// Point the refs at the directory their values are read from — the
    /// source stack's `--secrets-dir`, or `None` when none was given.
    pub fn bind(&mut self, dir: Option<&SecretsDir>) {
        self.dir = dir.cloned();
    }

    /// Read every bound secret from the directory, in binding order.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::NoSecretsDir`] when a secret is bound but no
    /// directory is, and the loader's refusal (naming the key, secret, path,
    /// and rule) when a file breaks a rule. Values read before a refusal are
    /// dropped, and so wiped, with the partial collection.
    pub fn load(&self) -> Result<Secrets, SecretError> {
        let mut loaded = Vec::with_capacity(self.entries.len());
        for (key, name) in &self.entries {
            let Some(dir) = &self.dir else {
                return Err(SecretError::NoSecretsDir { key: key.clone(), name: name.clone() });
            };
            let secret = dir.read_secret(name).map_err(|error| error.for_key(key))?;
            loaded.push((key.clone(), name.clone(), secret));
        }
        Ok(Secrets::from_entries(loaded))
    }
}

impl FromStr for SecretRefs {
    type Err = SecretError;

    /// Parse `<key>=<secret-name>[,…]`. Blank elements are skipped; every
    /// other element must hold a non-empty key free of whitespace and control
    /// characters, one `=`, and a valid [`SecretName`]. The refusal names the
    /// binding's position and the rule, never its text.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut entries = Vec::new();
        for (index, binding) in text.split(',').enumerate() {
            let binding = binding.trim();
            if binding.is_empty() {
                continue;
            }
            let position = index + 1;
            let refuse = |rule| SecretError::InvalidBinding { position, rule };
            let (key, name) = binding.split_once('=').ok_or_else(|| refuse("it has no `=`"))?;
            let key = key.trim();
            if key.is_empty() {
                return Err(refuse("its key is empty"));
            }
            if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(refuse("its key holds whitespace or a control character"));
            }
            let name = SecretName::try_from(name.trim()).map_err(|error| match error {
                SecretError::InvalidName { rule } => refuse(rule),
                other => other,
            })?;
            entries.push((key.to_owned(), name));
        }
        Ok(Self { entries, dir: None })
    }
}

/// The confique `parse_env` a `#[config(secrets)]` field wires on the env
/// side. An unset or empty variable yields no bindings.
///
/// # Errors
///
/// Returns the [`SecretError`] naming the first malformed binding, which
/// confique surfaces as a hard boot error (ADR-0090 §4).
pub fn parse_secret_refs(text: &str) -> Result<SecretRefs, SecretError> {
    text.parse()
}

/// Decodes the config-file form, a string in the same grammar, re-validating
/// every binding. There is no `Serialize`.
impl<'de> Deserialize<'de> for SecretRefs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::SecretRefs;

    #[test]
    fn refs_parse_every_binding_or_refuse() {
        let refs: SecretRefs = "a/x-api-key=one, b/bearer=two".parse().expect("two bindings parse");
        let entries: Vec<(&str, &str)> = refs.entries().map(|(key, name)| (key, name.as_str())).collect();
        assert_eq!(entries, [("a/x-api-key", "one"), ("b/bearer", "two")]);

        for refused in ["=n", "k=", "k", "k=../n", "a/x=one,k=", "a b=n"] {
            assert!(refused.parse::<SecretRefs>().is_err(), "{refused:?} must be refused");
        }
        assert_eq!("".parse::<SecretRefs>().expect("empty parses").entries().count(), 0, "no bindings is empty");
    }
}
