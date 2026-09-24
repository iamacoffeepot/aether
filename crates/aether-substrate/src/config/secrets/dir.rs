//! [`SecretsDir`]: the one directory secrets are read from (ADR-0235 §2), and
//! the loader rules every secret file must pass.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use super::error::SecretError;
use super::name::SecretName;
use super::secret::Secret;
use crate::config::ConfigError;

/// The largest secret file the loader reads, in bytes.
const MAX_SECRET_BYTES: u64 = 65_536;

/// The secrets directory: one file per secret, the file name is the secret's
/// name and the file bytes are its value — the systemd credentials, Docker
/// secrets, and Kubernetes secret-volume layout (ADR-0235 §2).
///
/// Only `--secrets-dir <path>` names it; the engine reads no environment
/// variable for it. A systemd unit passes `--secrets-dir %d`, which systemd
/// expands to the unit's credentials directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretsDir {
    path: PathBuf,
}

impl SecretsDir {
    /// Resolve the `--secrets-dir` flag. `None` when the flag is absent: the
    /// engine then has no secrets, and a binding that names one fails boot.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::SecretsDir`] when the path is not absolute or
    /// does not name a directory.
    pub fn locate(flag: Option<String>) -> Result<Option<Self>, ConfigError> {
        let Some(flag) = flag else {
            return Ok(None);
        };
        let path = PathBuf::from(flag);
        if !path.is_absolute() {
            return Err(ConfigError::SecretsDir { path, rule: "the path must be absolute" });
        }
        if !fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            return Err(ConfigError::SecretsDir { path, rule: "the path must name a directory" });
        }
        Ok(Some(Self { path }))
    }

    /// The directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read and check one secret file (ADR-0235 §2). The loader follows
    /// symlinks (Kubernetes mounts secrets as symlinks) and accepts a regular
    /// file of at most 65,536 bytes that group and others cannot write. It
    /// trims one trailing newline and accepts non-empty UTF-8 with no control
    /// characters.
    ///
    /// The buffer is a [`Secret`] before the first byte lands in it and is
    /// sized from the file's metadata, so the read never reallocates and a
    /// refused value is wiped with it.
    pub(super) fn read_secret(&self, name: &SecretName) -> Result<Secret, SecretError> {
        let path = self.path.join(name.as_str());
        let refuse = |rule| SecretError::Refused { key: None, name: name.clone(), path: path.clone(), rule };
        let unreadable = |source| SecretError::Unreadable { key: None, name: name.clone(), path: path.clone(), source };

        let meta = fs::metadata(&path).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(refuse("not a regular file"));
        }
        if meta.len() > MAX_SECRET_BYTES {
            return Err(refuse("larger than 65536 bytes"));
        }
        if group_or_others_can_write(&meta) {
            return Err(refuse("writable by group or others"));
        }

        let len = usize::try_from(meta.len()).map_err(|_| refuse("larger than 65536 bytes"))?;
        let mut secret = Secret::zeroed(len);
        let mut file = File::open(&path).map_err(unreadable)?;
        file.read_exact(secret.buffer_mut()).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => refuse("the file changed size while being read"),
            _ => unreadable(error),
        })?;
        let mut probe = [0u8; 1];
        if file.read(&mut probe).map_err(unreadable)? != 0 {
            return Err(refuse("the file changed size while being read"));
        }
        secret.finish().map_err(refuse)
    }

    /// The `--print-config` listing: the directory, then each validly named
    /// file with status `set` or `invalid: <rule>` from the loader's own
    /// checks. Every loaded value is dropped, and so wiped, at once; no value
    /// is ever part of the text (ADR-0235 §5).
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "dir: {} (from --secrets-dir)", self.path.display());
        let entries = match fs::read_dir(&self.path) {
            Ok(entries) => entries,
            Err(error) => {
                let _ = writeln!(out, "  cannot list the directory: {error}");
                return out;
            }
        };
        let mut names: Vec<SecretName> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().and_then(|name| SecretName::try_from(name).ok()))
            .collect();
        names.sort();
        if names.is_empty() {
            let _ = writeln!(out, "  (no validly named secret files)");
        }
        let width = names.iter().map(|name| name.as_str().len()).max().unwrap_or(0);
        for name in names {
            let status = match self.read_secret(&name) {
                Ok(_) => "set".to_owned(),
                Err(error) => format!("invalid: {}", error.rule()),
            };
            let _ = writeln!(out, "  {:<width$}  {status}", name.as_str());
        }
        out
    }
}

/// Whether the file's mode lets group or others write it — ssh's
/// `StrictModes` refusal of a tamperable key file. Readable by others is
/// accepted: Docker and Kubernetes mount secrets 0444 and 0644 by default.
#[cfg(unix)]
fn group_or_others_can_write(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o022 != 0
}

/// Non-unix hosts carry no group/other write bits to check.
#[cfg(not(unix))]
const fn group_or_others_can_write(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
pub(super) mod test_dir {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh, uniquely named directory under the system temp dir, removed on
    /// drop. Test values written here are obviously fake.
    pub(in crate::config::secrets) struct TempSecretsDir {
        pub path: PathBuf,
    }

    impl TempSecretsDir {
        pub fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("aether-secrets-{label}-{}-{unique}", process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create test secrets dir");
            Self { path }
        }

        /// Write `bytes` to `name` with mode 0600 (on unix).
        pub fn write(&self, name: &str, bytes: &[u8]) {
            let file = self.path.join(name);
            fs::write(&file, bytes).expect("write test secret");
            set_mode(&file, 0o600);
        }
    }

    impl Drop for TempSecretsDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    pub fn set_mode(file: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(file, fs::Permissions::from_mode(mode)).expect("set test secret mode");
    }

    #[cfg(not(unix))]
    pub fn set_mode(_file: &Path, _mode: u32) {}
}

#[cfg(test)]
mod tests {
    use super::SecretsDir;
    use super::test_dir::TempSecretsDir;
    use crate::config::secrets::name::SecretName;

    fn name(text: &str) -> SecretName {
        SecretName::try_from(text).expect("valid test name")
    }

    #[test]
    fn a_secret_file_is_trimmed_once_and_refused_when_unsafe() {
        let dir = TempSecretsDir::new("loader");
        let secrets = SecretsDir::locate(Some(dir.path.display().to_string())).expect("locate").expect("some dir");

        dir.write("echoed", b"fake-echoed-value\n");
        dir.write("crlf", b"fake-crlf-value\r\n");
        dir.write("twice", b"fake-twice-value\n\n");
        assert_eq!(secrets.read_secret(&name("echoed")).expect("echoed loads").expose(), "fake-echoed-value");
        assert_eq!(secrets.read_secret(&name("crlf")).expect("crlf loads").expose(), "fake-crlf-value");
        let twice = secrets.read_secret(&name("twice")).expect_err("only one newline is trimmed");

        dir.write("empty", b"\n");
        dir.write("injected", b"fake-injected\r\nX-Evil: fake-injected-2");
        dir.write("oversize", &vec![b'f'; 65_537]);
        dir.write("writable", b"fake-writable-value");
        super::test_dir::set_mode(&dir.path.join("writable"), 0o620);
        let mut refused = vec![
            ("twice", twice),
            ("empty", secrets.read_secret(&name("empty")).expect_err("empty is refused")),
            ("injected", secrets.read_secret(&name("injected")).expect_err("CR/LF inside is refused")),
            ("oversize", secrets.read_secret(&name("oversize")).expect_err("oversize is refused")),
            ("missing", secrets.read_secret(&name("missing")).expect_err("a missing file is refused")),
        ];
        if cfg!(unix) {
            refused.push(("writable", secrets.read_secret(&name("writable")).expect_err("group-writable is refused")));
        }

        for (label, error) in refused {
            let text = error.to_string();
            assert!(text.contains(label), "the refusal names the secret: {text}");
            assert!(!text.contains("fake-"), "the refusal never carries the content: {text}");
        }
        assert!(SecretsDir::locate(Some("relative/dir".to_owned())).is_err(), "a relative dir is refused");
        let file = dir.path.join("echoed").display().to_string();
        assert!(SecretsDir::locate(Some(file)).is_err(), "a file is not a secrets dir");
        assert!(SecretsDir::locate(None).expect("absent flag").is_none(), "no flag, no secrets");
    }

    #[test]
    fn print_config_status_never_contains_a_value() {
        let dir = TempSecretsDir::new("describe");
        dir.write("anthropic", b"fake-anthropic-value\n");
        dir.write("blank", b"");
        dir.write(".hidden", b"fake-hidden-value");
        let secrets = SecretsDir::locate(Some(dir.path.display().to_string())).expect("locate").expect("some dir");

        let text = secrets.describe();

        let anthropic = text.lines().find(|line| line.contains("anthropic")).expect("anthropic is listed");
        assert!(anthropic.ends_with("set"), "a valid secret reads `set`: {text}");
        let blank = text.lines().find(|line| line.contains("blank")).expect("blank is listed");
        assert!(blank.contains("invalid: the value is empty"), "a refused secret names its rule: {text}");
        assert!(!text.contains(".hidden"), "a file with an invalid name is not a secret: {text}");
        assert!(!text.contains("fake-"), "the listing never carries a value: {text}");
    }
}
