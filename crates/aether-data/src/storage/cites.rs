//! Per-type walk that collects every typed reference a value carries.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{DagId, KindId, MailboxId, ThreadId, TransformId};

/// One typed citation: the kind expected at `bytes` and the 32-byte digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Citation {
    /// Kind whose 8-byte prefix must head the cited blob.
    pub kind: KindId,
    /// Digest of the cited blob, prefix included.
    pub bytes: [u8; 32],
}

/// Accumulator for [`Cites::cites`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Citations {
    inner: Vec<Citation>,
}

impl Citations {
    /// Record one citation.
    pub fn push(&mut self, kind: KindId, bytes: [u8; 32]) {
        self.inner.push(Citation { kind, bytes });
    }

    /// Borrow the citations in visit order.
    #[must_use]
    pub fn as_slice(&self) -> &[Citation] {
        &self.inner
    }

    /// Take the citations in visit order.
    #[must_use]
    pub fn into_vec(self) -> Vec<Citation> {
        self.inner
    }
}

/// Collect every typed reference this value carries, at any depth.
pub trait Cites {
    /// Push this value's citations into `sink`.
    fn cites(&self, sink: &mut Citations);
}

macro_rules! empty_cites {
    ($($t:ty),+ $(,)?) => {
        $(
            impl Cites for $t {
                fn cites(&self, _sink: &mut Citations) {}
            }
        )+
    };
}

empty_cites!(
    u8,
    u16,
    u32,
    u64,
    i8,
    i16,
    i32,
    i64,
    f32,
    f64,
    bool,
    (),
    String,
    MailboxId,
    KindId,
    DagId,
    TransformId,
    ThreadId,
);

impl<T: Cites> Cites for Vec<T> {
    fn cites(&self, sink: &mut Citations) {
        for item in self {
            item.cites(sink);
        }
    }
}

impl<T: Cites, const N: usize> Cites for [T; N] {
    fn cites(&self, sink: &mut Citations) {
        for item in self {
            item.cites(sink);
        }
    }
}

impl<T: Cites> Cites for Option<T> {
    fn cites(&self, sink: &mut Citations) {
        if let Some(value) = self {
            value.cites(sink);
        }
    }
}

impl<K, V: Cites> Cites for BTreeMap<K, V> {
    fn cites(&self, sink: &mut Citations) {
        for value in self.values() {
            value.cites(sink);
        }
    }
}
