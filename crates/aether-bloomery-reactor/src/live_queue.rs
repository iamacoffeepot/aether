//! Portable, single-owner FIFO for selected live journal envelopes.
//!
//! Descriptor slots and byte storage are fixed. Payloads that cannot fit the
//! byte ring and descriptors beyond the fixed slots retain owned storage.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use aether_bloomery_kinds::JournalEntry;
use aether_data::KindId;

const DESCRIPTOR_CAPACITY: usize = 64;
const BYTE_CAPACITY: usize = 16 * 1024;

struct Descriptor {
    seq: u64,
    kind: KindId,
    cause: Option<u64>,
    recorded_at_millis: u64,
    storage: PayloadStorage,
}

enum PayloadStorage {
    Ring { offset: usize, len: usize, reclaim: usize },
    Owned(Vec<u8>),
}

/// Lossless FIFO whose common path stores payloads in one byte ring.
pub struct LiveQueue {
    descriptors: Vec<Option<Descriptor>>,
    overflow: VecDeque<Descriptor>,
    bytes: Vec<u8>,
    head: usize,
    len: usize,
    read: usize,
    write: usize,
    used: usize,
}

impl LiveQueue {
    /// Empty queue with fixed descriptor and byte backing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptors: (0..DESCRIPTOR_CAPACITY).map(|_| None).collect(),
            overflow: VecDeque::new(),
            bytes: vec![0; BYTE_CAPACITY],
            head: 0,
            len: 0,
            read: 0,
            write: 0,
            used: 0,
        }
    }

    /// Number of live entries awaiting preparation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len + self.overflow.len()
    }

    /// Whether there is no queued live entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Retain an entire envelope in receipt order.
    pub fn push(&mut self, entry: JournalEntry) {
        let JournalEntry { seq, kind, cause, recorded_at_millis, bytes } = entry;
        if self.len == DESCRIPTOR_CAPACITY || !self.overflow.is_empty() {
            self.overflow.push_back(Descriptor {
                seq,
                kind,
                cause,
                recorded_at_millis,
                storage: PayloadStorage::Owned(bytes),
            });
            return;
        }

        let storage = if let Some((offset, reclaim)) = self.reserve_bytes(bytes.len()) {
            self.bytes[offset..offset + bytes.len()].copy_from_slice(&bytes);
            PayloadStorage::Ring { offset, len: bytes.len(), reclaim }
        } else {
            PayloadStorage::Owned(bytes)
        };
        let index = (self.head + self.len) % DESCRIPTOR_CAPACITY;
        self.descriptors[index] = Some(Descriptor { seq, kind, cause, recorded_at_millis, storage });
        self.len += 1;
    }

    /// Remove the oldest envelope, reclaiming ring space or dropping its spill.
    pub fn pop(&mut self) -> Option<JournalEntry> {
        let descriptor = if self.len > 0 {
            let descriptor = self.descriptors[self.head].take()?;
            self.head = (self.head + 1) % DESCRIPTOR_CAPACITY;
            self.len -= 1;
            descriptor
        } else {
            self.overflow.pop_front()?
        };
        let bytes = match descriptor.storage {
            PayloadStorage::Owned(bytes) => bytes,
            PayloadStorage::Ring { offset, len, reclaim } => {
                let bytes = self.bytes[offset..offset + len].to_vec();
                self.used -= reclaim;
                self.read = (self.read + reclaim) % BYTE_CAPACITY;
                if self.used == 0 {
                    self.read = 0;
                    self.write = 0;
                }
                bytes
            }
        };
        Some(JournalEntry {
            seq: descriptor.seq,
            kind: descriptor.kind,
            cause: descriptor.cause,
            recorded_at_millis: descriptor.recorded_at_millis,
            bytes,
        })
    }

    fn reserve_bytes(&mut self, len: usize) -> Option<(usize, usize)> {
        if len == 0 || len > BYTE_CAPACITY - self.used {
            return None;
        }
        if self.used == 0 {
            self.read = 0;
            self.write = len % BYTE_CAPACITY;
            self.used = len;
            return Some((0, len));
        }
        if self.write >= self.read {
            if len <= BYTE_CAPACITY - self.write {
                let offset = self.write;
                self.write = (self.write + len) % BYTE_CAPACITY;
                self.used += len;
                return Some((offset, len));
            }
            let padding = BYTE_CAPACITY - self.write;
            let reclaim = padding.checked_add(len)?;
            if len <= self.read && reclaim <= BYTE_CAPACITY - self.used {
                self.write = len;
                self.used += reclaim;
                return Some((0, reclaim));
            }
        } else if len <= self.read - self.write {
            let offset = self.write;
            self.write += len;
            self.used += len;
            return Some((offset, len));
        }
        None
    }
}

impl Default for LiveQueue {
    fn default() -> Self {
        Self::new()
    }
}
