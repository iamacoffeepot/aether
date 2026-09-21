//! Injected-data sandbox a program runs against.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::str;
use core::task::{Context, Poll};

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, OpaqueBytes, ReadArtifactResult, Ref, Refusal, Utf8Text,
};
use aether_data::{Cites, Kind, KindId, Storage};

use crate::kinds::Detail;

/// Injected lookup and staging only: a miss is [`Refusal::InputMissing`], never a journal fetch.
#[derive(Clone, Copy)]
pub struct Sync;

/// Same injected map and staging as [`Sync`], plus awaitable journal `read` on a miss.
#[derive(Clone, Copy)]
pub struct Async;

struct Inner {
    closure: BTreeMap<Digest, ClosureArtifact>,
    staged: Vec<EncodedArtifact>,
    pending: Option<PendingArtifact>,
    terminal: BTreeMap<Digest, Refusal>,
}

/// One journal `ReadArtifact` the invocation child must send before polling again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingArtifact {
    /// Digest the program asked to fetch.
    pub digest: Digest,
    /// Kind the program's `read` / `read_text` expected.
    pub expected: KindId,
}

/// Owns the injected map for the life of one `run`. [`Env`] handles clone the pointer.
pub struct EnvOwner {
    inner: Box<RefCell<Inner>>,
}

impl EnvOwner {
    pub(crate) fn from_closure(closure: Vec<ClosureArtifact>) -> Self {
        let mut artifacts = BTreeMap::new();
        for artifact in closure {
            artifacts.insert(artifact.digest(), artifact);
        }
        Self {
            inner: Box::new(RefCell::new(Inner {
                closure: artifacts,
                staged: Vec::new(),
                pending: None,
                terminal: BTreeMap::new(),
            })),
        }
    }

    pub(crate) fn env<M>(&self) -> Env<M> {
        let ptr: *const RefCell<Inner> = &raw const *self.inner;
        Env { inner: ptr as usize, _mode: PhantomData }
    }
}

/// Sandbox parameterized by mode. Built from an [`aether_bloomery_kinds::Invoke`] closure.
#[derive(Clone, Copy)]
pub struct Env<M> {
    inner: usize,
    _mode: PhantomData<M>,
}

impl<M> Env<M> {
    fn cell(&self) -> &RefCell<Inner> {
        // SAFETY: `inner` is the `RefCell` inside an [`EnvOwner`] that outlives
        // every handle cloned from it — the local in a sync `run`, or the
        // `AsyncSession` that owns both the box and the boxed future.
        // SAFETY: `inner` is the address of the `RefCell` inside an [`EnvOwner`]
        // that outlives every handle cloned from it.
        unsafe { &*(self.inner as *const RefCell<Inner>) }
    }

    /// Load `r` from the injected closure.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent (or a journal fetch reported missing).
    /// [`Refusal::InputDecode`] when the kind prefix differs or the payload does not decode.
    pub fn injected<K: Storage>(&self, r: Ref<K>) -> Result<K, Refusal> {
        self.decode_injected(r.digest())
    }

    /// Load UTF-8 text `r` from the injected closure.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent (or a journal fetch reported missing).
    /// [`Refusal::InputDecode`] when the kind prefix differs or the payload is not UTF-8.
    pub fn injected_text(&self, r: Ref<Utf8Text>) -> Result<String, Refusal> {
        let inner = self.cell().borrow();
        if let Some(refusal) = inner.terminal.get(&r.digest()) {
            return Err(refusal.clone());
        }
        let artifact = inner.closure.get(&r.digest()).ok_or(Refusal::InputMissing)?;
        if artifact.kind() != Utf8Text::ID {
            return Err(Refusal::InputDecode);
        }
        str::from_utf8(artifact.bytes()).map(String::from).map_err(|_| Refusal::InputDecode)
    }

    /// Stage `payload` as [`OpaqueBytes`]. Identical payloads yield one artifact.
    pub fn stage_bytes(&mut self, payload: &[u8]) -> Ref<OpaqueBytes> {
        Ref::from_digest(self.record(EncodedArtifact::opaque_bytes(payload)))
    }

    /// Stage UTF-8 `text` as [`Utf8Text`]. Identical payloads yield one artifact.
    pub fn stage_text(&mut self, text: &str) -> Ref<Utf8Text> {
        Ref::from_digest(self.record(EncodedArtifact::text(text)))
    }

    /// Encode `value` and record it as a staged artifact.
    ///
    /// # Errors
    ///
    /// [`Refusal::Refused`] when encoding fails.
    pub fn stage_encoded<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, Refusal> {
        match EncodedArtifact::new(value) {
            Ok(encoded) => Ok(Ref::from_digest(self.record(encoded))),
            Err(error) => Err(Refusal::Refused { reason: Detail::new(format!("{error}")) }),
        }
    }

    pub(crate) fn staged(&self) -> Vec<EncodedArtifact> {
        self.cell().borrow().staged.clone()
    }

    pub(crate) fn into_staged(self) -> Vec<EncodedArtifact> {
        self.cell().borrow().staged.clone()
    }

    fn decode_injected<K: Storage>(&self, digest: Digest) -> Result<K, Refusal> {
        let inner = self.cell().borrow();
        if let Some(refusal) = inner.terminal.get(&digest) {
            return Err(refusal.clone());
        }
        let artifact = inner.closure.get(&digest).ok_or(Refusal::InputMissing)?;
        if artifact.kind() != K::ID {
            return Err(Refusal::InputDecode);
        }
        K::decode_storage(artifact.bytes()).map(|data| data.value).map_err(|_| Refusal::InputDecode)
    }

    fn record(&mut self, artifact: EncodedArtifact) -> Digest {
        let digest = artifact.digest();
        let mut inner = self.cell().borrow_mut();
        if inner.staged.iter().all(|existing| existing.digest() != digest) {
            inner.staged.push(artifact);
        }
        digest
    }
}

impl Env<Async> {
    /// Load `r` from the injected map, or fetch it from the journal on a miss.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent after the journal replies missing.
    /// [`Refusal::InputDecode`] when the kind prefix differs, the payload does not decode, or the
    /// journal reports a backend failure.
    pub async fn read<K: Storage>(&mut self, r: Ref<K>) -> Result<K, Refusal> {
        Read { env: *self, digest: r.digest(), requested: false, _kind: PhantomData }.await
    }

    /// Load UTF-8 text `r` from the injected map, or fetch it from the journal on a miss.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent after the journal replies missing.
    /// [`Refusal::InputDecode`] when the kind prefix differs, the payload is not UTF-8, or the
    /// journal reports a backend failure.
    pub async fn read_text(&mut self, r: Ref<Utf8Text>) -> Result<String, Refusal> {
        ReadText { env: *self, digest: r.digest(), requested: false }.await
    }

    pub(crate) fn take_pending(self) -> Option<PendingArtifact> {
        self.cell().borrow_mut().pending.take()
    }

    pub(crate) fn fail(self, digest: Digest, refusal: Refusal) {
        self.cell().borrow_mut().terminal.insert(digest, refusal);
    }

    fn terminal(self, digest: Digest) -> bool {
        self.cell().borrow().terminal.contains_key(&digest)
    }

    pub(crate) fn fulfill(self, result: ReadArtifactResult) {
        let mut inner = self.cell().borrow_mut();
        match result {
            ReadArtifactResult::Found { digest, kind, bytes } => {
                inner.terminal.remove(&digest);
                inner.closure.insert(digest, ClosureArtifact::new(kind, bytes));
            }
            ReadArtifactResult::Missing { digest } => {
                inner.terminal.insert(digest, Refusal::InputMissing);
            }
            ReadArtifactResult::Err { digest, .. } => {
                inner.terminal.insert(digest, Refusal::InputDecode);
            }
        }
    }

    fn request(self, digest: Digest, expected: KindId) {
        self.cell().borrow_mut().pending = Some(PendingArtifact { digest, expected });
    }
}

struct Read<K> {
    env: Env<Async>,
    digest: Digest,
    requested: bool,
    _kind: PhantomData<fn() -> K>,
}

impl<K: Storage> Future for Read<K> {
    type Output = Result<K, Refusal>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.env.decode_injected::<K>(this.digest) {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(refusal) if this.env.terminal(this.digest) => Poll::Ready(Err(refusal)),
            Err(Refusal::InputMissing) if !this.requested => {
                this.env.request(this.digest, K::ID);
                this.requested = true;
                Poll::Pending
            }
            Err(Refusal::InputMissing) => Poll::Pending,
            Err(refusal) => Poll::Ready(Err(refusal)),
        }
    }
}

struct ReadText {
    env: Env<Async>,
    digest: Digest,
    requested: bool,
}

impl Future for ReadText {
    type Output = Result<String, Refusal>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.env.injected_text(Ref::<Utf8Text>::from_digest(this.digest)) {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(refusal) if this.env.terminal(this.digest) => Poll::Ready(Err(refusal)),
            Err(Refusal::InputMissing) if !this.requested => {
                this.env.request(this.digest, Utf8Text::ID);
                this.requested = true;
                Poll::Pending
            }
            Err(Refusal::InputMissing) => Poll::Pending,
            Err(refusal) => Poll::Ready(Err(refusal)),
        }
    }
}
