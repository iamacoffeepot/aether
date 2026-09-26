//! Resolve one commit and read what it tracks: the recursive listing, then
//! every blob's bytes through a single `git cat-file --batch` child.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout};
use std::str;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};

use crate::git::{self, GitCommandError};

/// Everything one commit tracks.
pub(super) struct Listing {
    /// The full 40-hex sha the requested revision resolved to.
    pub(super) commit: String,
    /// Every entry, trees included, in `git ls-tree -r -t` order: each tree
    /// before its descendants.
    pub(super) entries: Vec<Listed>,
    /// Blob bytes by object id, one entry per distinct blob.
    pub(super) blobs: HashMap<String, Vec<u8>>,
}

/// One record of the recursive listing.
pub(super) struct Listed {
    /// The Git mode as git prints it, such as `100644` or `040000`.
    pub(super) mode: String,
    /// The object id.
    pub(super) oid: String,
    /// The `/`-separated path from the commit's root.
    pub(super) path: String,
}

/// Resolve `commit` in `repo` to a commit sha and read its listing and blobs.
///
/// # Errors
/// The revision names no commit, a path is not UTF-8, or git fails.
pub(super) fn read_commit(repo: &Path, commit: &str) -> Result<Listing> {
    if commit.starts_with('-') {
        bail!("`{commit}` is not a revision: a leading `-` would read as a git option");
    }

    let commit = git::run_ok(repo, &["rev-parse", "--verify", &format!("{commit}^{{commit}}")])?;
    let entries = list(repo, &commit)?;
    let blobs = read_blobs(repo, &entries)?;
    Ok(Listing { commit, entries, blobs })
}

/// Parse the NUL-separated `git ls-tree -r -t -z --full-tree` records.
fn list(repo: &Path, commit: &str) -> Result<Vec<Listed>> {
    let args = ["ls-tree", "-r", "-t", "-z", "--full-tree", commit];
    let output = git::run(repo, &args)?;
    if !output.status.success() {
        return Err(
            GitCommandError::Failed { args: format!("{args:?}"), stderr: git::trim_bytes(&output.stderr) }.into()
        );
    }

    output.stdout.split(|byte| *byte == 0).filter(|record| !record.is_empty()).map(parse_record).collect()
}

/// One `<mode> SP <type> SP <oid> TAB <path>` record.
fn parse_record(record: &[u8]) -> Result<Listed> {
    let tab = record.iter().position(|byte| *byte == b'\t').context("ls-tree record has no tab")?;
    let (meta, path) = (&record[..tab], &record[tab + 1..]);
    let path = String::from_utf8(path.to_vec())
        .map_err(|error| anyhow!("`{}`: the path is not UTF-8", String::from_utf8_lossy(error.as_bytes())))?;

    let meta = str::from_utf8(meta).with_context(|| format!("`{path}`: the ls-tree record is not text"))?;
    let mut fields = meta.split(' ');
    let (Some(mode), Some(_kind), Some(oid), None) = (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        bail!("`{path}`: malformed ls-tree record `{meta}`");
    };
    Ok(Listed { mode: mode.to_owned(), oid: oid.to_owned(), path })
}

/// Read every distinct blob the listing names through one `cat-file --batch`.
///
/// A scoped thread writes the requests while this thread reads the replies,
/// so neither side can fill a pipe the other is not draining.
fn read_blobs(repo: &Path, entries: &[Listed]) -> Result<HashMap<String, Vec<u8>>> {
    let mut seen = HashSet::new();
    let oids: Vec<&str> = entries
        .iter()
        .filter(|entry| is_blob_mode(&entry.mode))
        .map(|entry| entry.oid.as_str())
        .filter(|oid| seen.insert(*oid))
        .collect();

    let args = ["cat-file", "--batch"];
    let mut child = git::spawn_piped(repo, &args)?;
    let stdin = child.stdin.take().context("cat-file stdin was not piped")?;
    let stdout = child.stdout.take().context("cat-file stdout was not piped")?;

    let (read, written) = thread::scope(|scope| {
        let writer = scope.spawn(|| request(stdin, &oids));
        let read = reply(stdout, &oids);
        if read.is_err() {
            // Unblock a writer stuck on a pipe nobody reads any more.
            let _ = child.kill();
        }
        (read, writer.join())
    });

    let status = child.wait().context("waiting for git cat-file")?;
    let blobs = read?;
    written.map_err(|_| anyhow!("the cat-file request writer panicked"))??;
    if !status.success() {
        return Err(GitCommandError::Failed { args: format!("{args:?}"), stderr: String::new() }.into());
    }
    Ok(blobs)
}

/// The modes whose object is a blob: files, executables, and symlinks.
fn is_blob_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100755" | "120000")
}

/// Write one request line per oid, then close stdin so git exits.
fn request(stdin: ChildStdin, oids: &[&str]) -> Result<()> {
    let mut stdin = BufWriter::new(stdin);
    for oid in oids {
        writeln!(stdin, "{oid}").context("writing a cat-file request")?;
    }
    stdin.flush().context("flushing cat-file requests")
}

/// Read one `<oid> <type> <size>` header, the bytes, and the trailing newline
/// per requested oid, in request order.
fn reply(stdout: ChildStdout, oids: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
    let mut stdout = BufReader::new(stdout);
    let mut blobs = HashMap::with_capacity(oids.len());
    let mut header = String::new();
    for oid in oids {
        header.clear();
        stdout.read_line(&mut header).context("reading a cat-file header")?;

        let mut fields = header.trim_end().split(' ');
        let (Some(seen), Some("blob"), Some(size), None) = (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!("cat-file answered `{}` for blob {oid}", header.trim_end());
        };
        if seen != *oid {
            bail!("cat-file answered {seen} when {oid} was asked");
        }

        let size: usize = size.parse().with_context(|| format!("cat-file size `{size}` for {oid}"))?;
        let mut bytes = vec![0; size + 1];
        stdout.read_exact(&mut bytes).with_context(|| format!("reading blob {oid}"))?;
        if bytes.pop() != Some(b'\n') {
            bail!("cat-file blob {oid} was not followed by a newline");
        }
        blobs.insert((*oid).to_owned(), bytes);
    }
    Ok(blobs)
}
