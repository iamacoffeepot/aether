//! Archives written by other tools decode to the expected tree, and
//! re-encode to that tree's canonical bytes.
//!
//! The fixtures were made with GNU tar 1.35 and Docker 29.6, with `umask 022`,
//! `L1` sixty `d`s, and `L2` sixty `f`s followed by `.txt`:
//!
//! ```sh
//! # gnu.tar: GNU format, a `./` prefix, a GNU `L` long name, a hardlink,
//! # a symlink, an empty directory, and a non-root owner.
//! mkdir -p gnu/bin gnu/empty gnu/long/$L1
//! printf 'hello\n' > gnu/hello.txt
//! printf '#!/bin/sh\necho run\n' > gnu/bin/run.sh; chmod 755 gnu/bin/run.sh
//! ln gnu/bin/run.sh gnu/bin/same.sh
//! ln -s hello.txt gnu/link
//! printf 'long\n' > gnu/long/$L1/$L2
//! tar --format=gnu --sort=name --mtime=@0 --owner=1234 --group=1234 --numeric-owner -cf gnu.tar -C gnu .
//!
//! # posix.tar: POSIX format over an explicit file list with no parent
//! # directories, so every parent is implicit; GNU tar writes PAX `atime`
//! # and `ctime` records for every entry and `path` for the long and the
//! # non-ASCII name.
//! mkdir -p posix/bin posix/long/$L1
//! printf 'hello\n' > posix/hello.txt
//! printf '#!/bin/sh\necho run\n' > posix/bin/run.sh; chmod 755 posix/bin/run.sh
//! printf 'café\n' > posix/café.txt
//! printf 'long\n' > posix/long/$L1/$L2
//! tar --format=posix --mtime=@0 --owner=0 --group=0 --numeric-owner -cf posix.tar -C posix --no-recursion \
//!   bin/run.sh café.txt hello.txt long/$L1/$L2
//!
//! # docker-work.tar: a tree streamed into a container with a tmpfs at
//! # /work/target, a scratch file written into the tmpfs, then /work read
//! # back through the archive endpoint.
//! mkdir -p in/bin
//! printf 'hello\n' > in/hello.txt
//! printf '#!/bin/sh\necho run\n' > in/bin/run.sh; chmod 755 in/bin/run.sh
//! ln -s hello.txt in/link
//! tar --format=posix --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf in.tar -C in .
//! cid=$(docker create --tmpfs /work/target alpine:latest sleep 600); docker start $cid
//! curl -sf --unix-socket /var/run/docker.sock -X PUT -H 'Content-Type: application/x-tar' \
//!   --data-binary @in.tar "http://localhost/containers/$cid/archive?path=/work"
//! docker exec $cid sh -c 'echo scratch > /work/target/junk'
//! curl -sf --unix-socket /var/run/docker.sock -o docker-work.tar \
//!   "http://localhost/containers/$cid/archive?path=/work"
//! docker rm -f $cid
//! ```

mod common;

use aether_bloomery_kinds::{Node, Path, Ref, Tree};
use common::MemoryStore;

const GNU: &[u8] = include_bytes!("fixtures/gnu.tar");
const POSIX: &[u8] = include_bytes!("fixtures/posix.tar");
const DOCKER_WORK: &[u8] = include_bytes!("fixtures/docker-work.tar");

fn run_script(store: &mut MemoryStore) -> Node {
    Node::Executable(store.add_blob(b"#!/bin/sh\necho run\n"))
}

fn hello(store: &mut MemoryStore) -> Node {
    Node::File(store.add_blob(b"hello\n"))
}

fn link() -> Node {
    Node::Symlink(Path::new("hello.txt").expect("valid target"))
}

/// `long/<60 d>/<60 f>.txt`, a path past the 100-byte ustar name field.
fn long(store: &mut MemoryStore) -> Node {
    let file = Node::File(store.add_blob(b"long\n"));
    let leaf = format!("{}.txt", "f".repeat(60));
    let inner = Node::Directory(store.add_dir(vec![(leaf.as_str(), file)]));
    Node::Directory(store.add_dir(vec![("d".repeat(60).as_str(), inner)]))
}

/// Decode `fixture` and compare with `expected`. Equal references mean
/// `encode(decode(fixture))` is `expected`'s canonical bytes.
fn assert_decodes_to(store: &mut MemoryStore, fixture: &[u8], expected: Ref<Tree>) {
    assert_eq!(store.decode(fixture), expected);
}

#[test]
fn a_gnu_format_archive_decodes_with_its_long_name_and_hardlink() {
    // Catches a dropped GNU `L` name, a `./` prefix kept as a segment, the
    // root entry refused, a hardlink not resolved to its target's node, or a
    // non-root owner leaking into the tree.
    let mut store = MemoryStore::default();
    let run = run_script(&mut store);
    let bin = Node::Directory(store.add_dir(vec![("run.sh", run.clone()), ("same.sh", run)]));
    let empty = Node::Directory(store.add_dir(vec![]));
    let hello = hello(&mut store);
    let long = long(&mut store);
    let expected =
        store.add_dir(vec![("bin", bin), ("empty", empty), ("hello.txt", hello), ("link", link()), ("long", long)]);

    assert_decodes_to(&mut store, GNU, expected);
}

#[test]
fn a_posix_format_archive_decodes_with_pax_paths_and_implicit_parents() {
    // Catches a PAX `path` not applied, an ignored PAX key refused, or a
    // missing parent refused instead of created.
    let mut store = MemoryStore::default();
    let run = run_script(&mut store);
    let bin = Node::Directory(store.add_dir(vec![("run.sh", run)]));
    let cafe = Node::File(store.add_blob("café\n".as_bytes()));
    let hello = hello(&mut store);
    let long = long(&mut store);
    let expected = store.add_dir(vec![("bin", bin), ("café.txt", cafe), ("hello.txt", hello), ("long", long)]);

    assert_decodes_to(&mut store, POSIX, expected);
}

#[test]
fn a_docker_archive_read_decodes_with_the_scratch_mount_empty() {
    // Catches a decoder that rejects what the container runtime's archive
    // endpoint returns: its `work/` prefix, its root directory entry, and
    // the tmpfs scratch mount it returns as an empty directory.
    let mut store = MemoryStore::default();
    let run = run_script(&mut store);
    let bin = Node::Directory(store.add_dir(vec![("run.sh", run)]));
    let hello = hello(&mut store);
    let target = Node::Directory(store.add_dir(vec![]));
    let work =
        Node::Directory(store.add_dir(vec![("bin", bin), ("hello.txt", hello), ("link", link()), ("target", target)]));
    let expected = store.add_dir(vec![("work", work)]);

    assert_decodes_to(&mut store, DOCKER_WORK, expected);
}
