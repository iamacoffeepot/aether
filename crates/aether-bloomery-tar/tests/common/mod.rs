//! An in-memory store implementing both codec traits, plus tree builders.

use std::collections::{BTreeMap, HashMap};

use aether_bloomery_kinds::{Name, Node, OpaqueBytes, Ref, Tree};
use aether_bloomery_tar::{BlobWriter, SourceBlob, TreeSink, TreeSource, decode};

/// Blobs and trees keyed by digest, the way a journal keys them.
#[derive(Default)]
pub struct MemoryStore {
    blobs: HashMap<[u8; 32], Vec<u8>>,
    trees: HashMap<[u8; 32], Tree>,
}

impl MemoryStore {
    /// Store `bytes` as a blob.
    pub fn add_blob(&mut self, bytes: &[u8]) -> Ref<OpaqueBytes> {
        let blob = Ref::of_bytes(bytes);
        self.blobs.insert(*blob.digest().as_bytes(), bytes.to_vec());
        blob
    }

    /// Store a directory of `entries`.
    pub fn add_dir(&mut self, entries: Vec<(&str, Node)>) -> Ref<Tree> {
        let entries = entries
            .into_iter()
            .map(|(name, node)| (Name::new(name).expect("valid name"), node))
            .collect::<BTreeMap<_, _>>();
        self.put_tree(&Tree::new(entries).expect("no collision")).expect("a tree encodes")
    }

    /// Decode `bytes` into this store.
    pub fn decode(&mut self, bytes: &[u8]) -> Ref<Tree> {
        decode(bytes, self).expect("the archive decodes")
    }
}

impl TreeSource for MemoryStore {
    type Error = String;
    type Blob<'a> = &'a [u8];

    fn tree(&mut self, tree: &Ref<Tree>) -> Result<Tree, String> {
        self.trees.get(tree.digest().as_bytes()).cloned().ok_or_else(|| format!("no tree {}", tree.digest()))
    }

    fn blob(&mut self, blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<&[u8]>, String> {
        let bytes = self.blobs.get(blob.digest().as_bytes()).ok_or_else(|| format!("no blob {}", blob.digest()))?;
        Ok(SourceBlob { len: bytes.len() as u64, reader: bytes.as_slice() })
    }
}

impl TreeSink for MemoryStore {
    type Error = String;
    type Blob<'a> = MemoryBlob<'a>;

    /// Grows as chunks arrive: `len` is the archive's unverified claim.
    fn begin_blob(&mut self, _len: u64) -> Result<MemoryBlob<'_>, String> {
        Ok(MemoryBlob { store: self, bytes: Vec::new() })
    }

    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, String> {
        let reference = Ref::of_encoded(tree).map_err(|error| format!("{error:?}"))?;
        self.trees.insert(*reference.digest().as_bytes(), tree.clone());
        Ok(reference)
    }
}

/// One blob being written into a [`MemoryStore`].
pub struct MemoryBlob<'a> {
    store: &'a mut MemoryStore,
    bytes: Vec<u8>,
}

impl BlobWriter for MemoryBlob<'_> {
    type Error = String;

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(self) -> Result<Ref<OpaqueBytes>, String> {
        Ok(self.store.add_blob(&self.bytes))
    }
}
