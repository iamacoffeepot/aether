//! Injected-data sandbox a program runs against.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::str;
use core::task::{Context, Poll};

use aether_actor::{Addressable, CallerAddressable, Replies, ReplyMode, Sends, Singleton, WasmCtx};
use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, OpaqueBytes, ReadArtifactResult, Ref, Refusal, Utf8Text,
};
use aether_data::{Cites, Kind, KindId, Storage};

use crate::kinds::Detail;

/// Injected lookup and staging only: a miss is [`Refusal::InputMissing`], never a journal fetch.
#[derive(Clone, Copy)]
pub struct Sync;

/// Same injected map and staging as [`Sync`], plus an awaitable `read` that fetches a miss
/// through the bundle root and the driver, which forwards it to the journal.
#[derive(Clone, Copy)]
pub struct Async;

struct Inner {
    closure: BTreeMap<Digest, ClosureArtifact>,
    staged: Vec<EncodedArtifact>,
    pending: Option<Pending>,
    call_reply: Option<Result<(KindId, Vec<u8>), Refusal>>,
    terminal: BTreeMap<Digest, Refusal>,
}

/// One `ReadArtifact` the invocation child must send to its bundle root before polling again.
///
/// The root relays it to the driver, which forwards it to the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingArtifact {
    /// Digest the program asked to fetch.
    pub digest: Digest,
    /// Kind the program's `read` / `read_text` expected.
    pub expected: KindId,
}

/// One cap send the invocation child must emit before polling again.
///
/// `mailbox` / `kind_id` / `expected_reply` are the pump's type-erased
/// view. The request value stays `K` inside [`Self::dispatch`], which
/// sends it to the binding's target by type.
pub struct PendingCall {
    /// `Addressable::NAMESPACE` of the binding's target actor.
    pub mailbox: &'static str,
    /// Kind id of the captured request.
    pub kind_id: KindId,
    /// Kind id the `#[fallback]` must match before resume.
    pub expected_reply: KindId,
    body: Box<dyn DispatchBody>,
}

trait DispatchBody: Send {
    fn send(&self, sends: &mut Sends<'_>);
}

struct CapturedSend<A, K> {
    mail: K,
    _target: PhantomData<fn() -> A>,
}

impl<A, K> DispatchBody for CapturedSend<A, K>
where
    A: Singleton + CallerAddressable + Replies<K>,
    K: Kind + Send,
{
    fn send(&self, sends: &mut Sends<'_>) {
        sends.actor::<A>().send(&self.mail);
    }
}

impl PendingCall {
    fn new<A, K>(mail: K) -> Self
    where
        A: Singleton + CallerAddressable + Replies<K> + 'static,
        K: Kind + Send + 'static,
    {
        Self {
            mailbox: A::NAMESPACE,
            kind_id: K::ID,
            expected_reply: A::Reply::ID,
            body: Box::new(CapturedSend::<A, K> { mail, _target: PhantomData }),
        }
    }

    /// Send the captured request to the binding's target through a typed send.
    ///
    /// The program chose the target at run time, when its binding captured
    /// the call, and the captured body hides the target and kind behind a
    /// trait object. A trait object cannot carry a method generic over the
    /// sending actor, so this erases the sender's view once, here. The
    /// target still answers the kind: `T: Replies<K>` was checked when the
    /// call was captured. The bundle's allowlist, built from its declared
    /// APIs, refuses any undeclared target before this call. #6469 replaces
    /// the erased view with a send that the invocation's declared
    /// dependencies prove.
    pub fn dispatch<A, M: ReplyMode>(&self, ctx: &mut WasmCtx<'_, A, M>) {
        self.body.send(&mut ctx.erase().sends());
    }
}

impl fmt::Debug for PendingCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingCall")
            .field("mailbox", &self.mailbox)
            .field("kind_id", &self.kind_id)
            .field("expected_reply", &self.expected_reply)
            .finish_non_exhaustive()
    }
}

/// First-poll wait the invocation child must discharge.
#[derive(Debug)]
pub enum Pending {
    /// Journal `ReadArtifact` for [`PendingArtifact::digest`].
    Artifact(PendingArtifact),
    /// Cap send; `K` stays captured until [`PendingCall::dispatch`].
    Send(PendingCall),
}

/// Trailing `run` argument constructed from [`Env<Async>`].
pub trait InjectedApi: Sized {
    /// Actor this binding may send to.
    type Target: Addressable;
    /// Sampled APIs cannot pair with [`crate::kinds::Mode::Pure`].
    const SAMPLED: bool;
    /// Build an unforgeable handle from the invocation's environment.
    fn from_env(env: &mut Env<Async>) -> Self;
}

/// Generic actor handle sharing the invocation environment pointer with [`Env<Async>`].
pub struct Binding<A: Addressable> {
    env: Env<Async>,
    _target: PhantomData<A>,
}

impl<A: Addressable> InjectedApi for Binding<A> {
    type Target = A;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self { env: *env, _target: PhantomData }
    }
}

impl<A: Addressable> Binding<A> {
    /// Send `mail` and await `<A as Replies<K>>::Reply`.
    pub fn call<K>(
        &mut self,
        mail: K,
    ) -> impl Future<Output = Result<<A as Replies<K>>::Reply, Refusal>> + Send + 'static
    where
        A: Singleton + CallerAddressable + Replies<K> + Unpin + 'static,
        K: Kind + Send + Unpin + 'static,
    {
        Call::<A, K> { env: self.env, mail: Some(mail), _target: PhantomData }
    }
}

/// Sampled HTTP sugar over [`Binding<aether_http::HttpCapability>`].
pub struct Http(Binding<aether_http::HttpCapability>);

impl InjectedApi for Http {
    type Target = aether_http::HttpCapability;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self(Binding::from_env(env))
    }
}

impl Http {
    /// Await [`aether_http::FetchResult`] for `mail`.
    pub fn fetch(
        &mut self,
        mail: aether_http::Fetch,
    ) -> impl Future<Output = Result<aether_http::FetchResult, Refusal>> + Send + 'static {
        self.0.call(mail)
    }
}

/// Sampled process sugar over [`Binding<aether_process::ProcessCapability>`].
pub struct Process(Binding<aether_process::ProcessCapability>);

impl InjectedApi for Process {
    type Target = aether_process::ProcessCapability;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self(Binding::from_env(env))
    }
}

impl Process {
    /// Await [`aether_process::RunResult`] for `mail`.
    pub fn run(
        &mut self,
        mail: aether_process::Run,
    ) -> impl Future<Output = Result<aether_process::RunResult, Refusal>> + Send + 'static {
        self.0.call(mail)
    }
}

struct Call<A, K> {
    env: Env<Async>,
    mail: Option<K>,
    _target: PhantomData<A>,
}

impl<A, K> Future for Call<A, K>
where
    A: Singleton + CallerAddressable + Replies<K> + Unpin + 'static,
    K: Kind + Send + Unpin + 'static,
{
    type Output = Result<A::Reply, Refusal>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.env.take_call_reply() {
            return Poll::Ready(decode_call_reply::<A::Reply>(result));
        }
        if let Some(mail) = this.mail.take() {
            this.env.request_send(PendingCall::new::<A, K>(mail));
            return Poll::Pending;
        }
        Poll::Pending
    }
}

fn decode_call_reply<R: Kind>(result: Result<(KindId, Vec<u8>), Refusal>) -> Result<R, Refusal> {
    let (kind, bytes) = result?;
    if kind != R::ID {
        return Err(Refusal::InputDecode);
    }
    R::decode_from_bytes(&bytes).ok_or(Refusal::InputDecode)
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
                call_reply: None,
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

    pub(crate) fn take_pending(self) -> Option<Pending> {
        self.cell().borrow_mut().pending.take()
    }

    pub(crate) fn take_call_reply(self) -> Option<Result<(KindId, Vec<u8>), Refusal>> {
        self.cell().borrow_mut().call_reply.take()
    }

    pub(crate) fn fulfill_call(self, kind: KindId, bytes: Vec<u8>) {
        self.cell().borrow_mut().call_reply = Some(Ok((kind, bytes)));
    }

    pub(crate) fn reject_call(self, refusal: Refusal) {
        self.cell().borrow_mut().call_reply = Some(Err(refusal));
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
        self.cell().borrow_mut().pending = Some(Pending::Artifact(PendingArtifact { digest, expected }));
    }

    fn request_send(self, pending: PendingCall) {
        self.cell().borrow_mut().pending = Some(Pending::Send(pending));
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
