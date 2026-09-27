//! A container image named by digest, never by tag. Valid by construction.

use alloc::string::String;

const IMAGE_REF_MAX_BYTES: usize = 255;
const DIGEST_PREFIX: &str = "sha256:";
const DIGEST_HEX_LEN: usize = 64;

/// Why [`ImageRef::new`] or decode refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRefError {
    /// The string was longer than 255 bytes.
    TooLong,
    /// The string had no `@` before a digest.
    MissingDigest,
    /// The digest was not `sha256:` and 64 lowercase hex digits.
    Digest,
    /// A repository component carried a `:tag`. A tag moves; a digest does not.
    Tag,
    /// The registry host was not dot-separated lowercase labels.
    Host,
    /// The registry port was not a number from 0 to 65535.
    Port,
    /// A repository component was empty or broke `[a-z0-9]+([._-][a-z0-9]+)*`.
    Component,
}

impl ImageRefError {
    const fn reason(self) -> &'static str {
        match self {
            Self::TooLong => "too-long",
            Self::MissingDigest => "missing-digest",
            Self::Digest => "digest",
            Self::Tag => "tag",
            Self::Host => "host",
            Self::Port => "port",
            Self::Component => "component",
        }
    }
}

invariant_errors!(ImageRefError);

/// `<repository>@sha256:<64 lowercase hex>`, at most 255 bytes.
///
/// The repository is an optional registry `host[:port]/` followed by one or
/// more lowercase components joined by `/`. As in Docker, the first of
/// several components is the registry exactly when it holds a `.` or a `:`,
/// or is `localhost`. The host is lowercase so an image has one spelling.
///
/// There is no tag: a tag can move, and an imported tree must be a function
/// of the request that named it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ImageRef(String);

impl ImageRef {
    /// Accept a digest-pinned image reference.
    ///
    /// # Errors
    ///
    /// [`ImageRefError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, ImageRefError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the whole reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The repository, registry included: everything before the `@`.
    #[must_use]
    pub fn repository(&self) -> &str {
        self.split().0
    }

    /// The digest, `sha256:` included: everything after the `@`.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.split().1
    }

    fn split(&self) -> (&str, &str) {
        self.0.split_once('@').unwrap_or((self.0.as_str(), ""))
    }

    fn check(value: &str) -> Result<(), ImageRefError> {
        if value.len() > IMAGE_REF_MAX_BYTES {
            return Err(ImageRefError::TooLong);
        }
        let Some((repository, digest)) = value.split_once('@') else {
            return Err(ImageRefError::MissingDigest);
        };
        check_digest(digest)?;

        let mut components = repository.split('/').peekable();
        let first = components.next().unwrap_or_default();
        let has_registry = components.peek().is_some() && (first.contains(['.', ':']) || first == "localhost");
        if has_registry {
            check_registry(first)?;
        } else {
            check_component(first)?;
        }
        components.try_for_each(check_component)
    }
}

fn check_digest(digest: &str) -> Result<(), ImageRefError> {
    let hex = digest.strip_prefix(DIGEST_PREFIX).ok_or(ImageRefError::Digest)?;
    if hex.len() == DIGEST_HEX_LEN && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(ImageRefError::Digest)
    }
}

fn check_registry(registry: &str) -> Result<(), ImageRefError> {
    let (host, port) = match registry.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (registry, None),
    };
    if !host.split('.').all(is_host_label) {
        return Err(ImageRefError::Host);
    }
    match port {
        Some(port) if !port.bytes().all(|byte| byte.is_ascii_digit()) || port.parse::<u16>().is_err() => {
            Err(ImageRefError::Port)
        }
        _ => Ok(()),
    }
}

/// `[a-z0-9]`, optionally with `[a-z0-9-]*` between and `[a-z0-9]` at the end.
fn is_host_label(label: &str) -> bool {
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = label.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(&first), Some(&last)) => {
            alnum(first) && alnum(last) && bytes.iter().all(|&byte| alnum(byte) || byte == b'-')
        }
        _ => false,
    }
}

/// `[a-z0-9]+([._-][a-z0-9]+)*`: lowercase runs joined by single separators.
fn check_component(component: &str) -> Result<(), ImageRefError> {
    if component.contains(':') {
        return Err(ImageRefError::Tag);
    }
    let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let valid = component.split(['.', '_', '-']).all(|run| !run.is_empty() && run.bytes().all(alnum));
    if valid {
        Ok(())
    } else {
        Err(ImageRefError::Component)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::String;

    use super::{IMAGE_REF_MAX_BYTES, ImageRef, ImageRefError};
    use crate::kinds::test_support::assert_rule;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn pinned(repository: &str) -> String {
        format!("{repository}@sha256:{HEX}")
    }

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let digest_len = "@sha256:".len() + HEX.len();
        let too_long = pinned(&"a".repeat(IMAGE_REF_MAX_BYTES + 1 - digest_len));
        let max_len = pinned(&"a".repeat(IMAGE_REF_MAX_BYTES - digest_len));
        let upper_hex = format!("debian@sha256:{}", HEX.to_uppercase());
        let short_hex = format!("debian@sha256:{}", &HEX[1..]);
        let cases = [
            (too_long, ImageRefError::TooLong, max_len),
            (String::from("debian"), ImageRefError::MissingDigest, pinned("debian")),
            (upper_hex, ImageRefError::Digest, pinned("debian")),
            (short_hex, ImageRefError::Digest, pinned("debian")),
            (format!("debian@sha512:{HEX}"), ImageRefError::Digest, pinned("debian")),
            (pinned("debian:bookworm"), ImageRefError::Tag, pinned("debian")),
            (pinned("ghcr.io/rust:1.97"), ImageRefError::Tag, pinned("ghcr.io/rust")),
            (pinned("Registry.io/rust"), ImageRefError::Host, pinned("registry.io/rust")),
            (pinned("-registry.io/rust"), ImageRefError::Host, pinned("r-egistry.io/rust")),
            (pinned("localhost:65536/rust"), ImageRefError::Port, pinned("localhost:65535/rust")),
            (pinned("localhost:+50/rust"), ImageRefError::Port, pinned("localhost:50/rust")),
            (pinned("Debian"), ImageRefError::Component, pinned("debian")),
            (pinned("library/de__bian"), ImageRefError::Component, pinned("library/de_bian")),
            (pinned("library/debian-"), ImageRefError::Component, pinned("library/debian-1")),
            (pinned("library//debian"), ImageRefError::Component, pinned("library/debian")),
            (pinned(""), ImageRefError::Component, pinned("a")),
        ];
        for (reject, error, accept) in cases {
            assert_rule(ImageRef::new, reject, error, accept);
        }
    }

    #[test]
    fn a_lone_component_is_never_a_registry() {
        // Catches a registry test that skips the "more than one component"
        // half: it would read `debian_slim.v2` as a host, whose labels refuse
        // `_`, and refuse a valid one-component repository.
        let image = ImageRef::new(pinned("debian_slim.v2")).expect("a one-component repository");
        assert_eq!((image.repository(), image.digest()), ("debian_slim.v2", &*format!("sha256:{HEX}")));
    }
}
