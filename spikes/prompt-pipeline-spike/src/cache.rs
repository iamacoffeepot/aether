use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn get_or_compute<F>(&self, key_parts: &[&str], compute: F) -> Result<String>
    where
        F: FnOnce() -> Result<String>,
    {
        let key = hash_key(key_parts);
        let path = self.path_for(&key);

        if path.exists() {
            return fs::read_to_string(&path)
                .with_context(|| format!("reading cache hit {}", path.display()));
        }

        let value = compute()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }
        fs::write(&path, &value)
            .with_context(|| format!("writing cache miss {}", path.display()))?;
        Ok(value)
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let (head, tail) = key.split_at(2);
        self.root.join(head).join(format!("{tail}.txt"))
    }
}

/// Binary-bytes cache, parallel to `Cache` for text outputs. Used for
/// image-gen results (PNG/JPEG bytes) where the artifact is megabytes
/// of binary, not utf-8 prose.
pub struct BinaryCache {
    root: PathBuf,
}

impl BinaryCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn get_or_compute<F>(
        &self,
        key_parts: &[&str],
        ext: &str,
        compute: F,
    ) -> Result<(PathBuf, Vec<u8>)>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        let key = hash_key(key_parts);
        let path = self.path_for(&key, ext);

        if path.exists() {
            let bytes = fs::read(&path)
                .with_context(|| format!("reading cache hit {}", path.display()))?;
            return Ok((path, bytes));
        }

        let value = compute()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }
        fs::write(&path, &value)
            .with_context(|| format!("writing cache miss {}", path.display()))?;
        Ok((path, value))
    }

    fn path_for(&self, key: &str, ext: &str) -> PathBuf {
        let (head, tail) = key.split_at(2);
        self.root.join(head).join(format!("{tail}.{ext}"))
    }
}

fn hash_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"\0");
    }
    format!("{:x}", h.finalize())
}
