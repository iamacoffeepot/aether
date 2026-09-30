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
use core::task::{Context, Poll};

use aether_actor::{Addressable, CallerAddressable, Replies, Singleton};
use aether_bloomery_kinds::{
    ApiCall, ClosureArtifact, Digest, EncodedArtifact, ErasedRef, ExecutorFault, OpaqueBytes, ProgramApi,
    ReadArtifactResult, Ref, Refusal, Utf8Text,
};
use aether_data::{ActorMail, Cites, Kind, KindId, Storage};

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
    ended: Option<ExecutorFault>,
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

/// One API call the invocation child must relay before polling again.
///
/// The invocation sends it to its bundle root as [`ApiCall`] (see
/// [`Self::api_call`]); the root relays it to the driver that sent the
/// `Invoke`, which maps [`Self::api`] to a provider it holds or refuses it
/// (ADR-0240 D6). The request was encoded once, at capture, where
/// `A: Replies<K>` typed it.
pub struct PendingCall {
    /// The API the program's binding captured the call through.
    pub api: ProgramApi,
    /// Kind id of the captured request.
    pub kind_id: KindId,
    /// Kind id the relayed reply must carry before resume.
    pub expected_reply: KindId,
    payload: Vec<u8>,
}

impl PendingCall {
    /// Capture `mail`, the payload the driver relays as the provider's row
    /// `K`, whose reply `A: Replies<K>` types. For most calls the payload is
    /// the row itself (`P = K`); a `Workspace` call's payload is the
    /// `RunRequest` the driver sends on as a `Run`.
    fn new<A, K, P>(api: ProgramApi, mail: &P) -> Self
    where
        A: Replies<K>,
        K: ActorMail,
        P: ActorMail,
    {
        Self { api, kind_id: P::ID, expected_reply: <A as Replies<K>>::Reply::ID, payload: mail.encode_into_bytes() }
    }

    /// The mail that relays this call as the invocation's `call`th.
    #[must_use]
    pub fn api_call(&self, call: u64) -> ApiCall {
        ApiCall { call, api: self.api, kind: self.kind_id, payload: self.payload.clone() }
    }
}

impl fmt::Debug for PendingCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingCall")
            .field("api", &self.api)
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
    /// API call, relayed through the bundle root as [`PendingCall::api_call`].
    Send(PendingCall),
}

mod sealed {
    /// Closes [`super::InjectedApi`] to the APIs this crate names.
    pub trait Sealed {}

    impl Sealed for super::Http {}
    impl Sealed for super::Process {}
    impl Sealed for super::Workspace {}
}

/// Trailing `run` argument constructed from [`Env<Async>`].
///
/// The set is closed: [`Http`], [`Process`], and [`Workspace`] are its only
/// members, and the trait is sealed. `#[program]` accepts a trailing binding
/// only by one of those names, maps the name to its target capability through
/// the table in `__macro_internals::api_target`, and emits a check at the
/// parameter that this trait's [`Self::Target`] is the table's type. The
/// bundle's invocation declares no dependency: it relays a captured call
/// through its root to the driver that invoked it, which maps the API to a
/// provider it holds (ADR-0240 D6).
pub trait InjectedApi: sealed::Sealed + Sized {
    /// Actor whose reply contract types this binding's calls.
    type Target: Addressable;
    /// Sampled APIs cannot pair with [`crate::kinds::Mode::Pure`].
    const SAMPLED: bool;
    /// Build an unforgeable handle from the invocation's environment.
    fn from_env(env: &mut Env<Async>) -> Self;
}

/// Actor handle sharing the invocation environment pointer with [`Env<Async>`]:
/// the one implementation [`Http`], [`Process`], and [`Workspace`] share.
struct Binding<A: Addressable> {
    env: Env<Async>,
    api: ProgramApi,
    _target: PhantomData<A>,
}

impl<A: Addressable> Binding<A> {
    /// Share the invocation's environment pointer, capturing calls as `api`.
    fn new(env: &mut Env<Async>, api: ProgramApi) -> Self {
        Self { env: *env, api, _target: PhantomData }
    }

    /// Send `mail` and await `<A as Replies<K>>::Reply`.
    fn call<K>(&mut self, mail: K) -> Call<A, K, K>
    where
        A: Singleton + CallerAddressable + Replies<K> + Unpin + 'static,
        K: ActorMail + Send + Unpin + 'static,
    {
        self.relay::<K, K>(mail)
    }

    /// Send `mail`, which the driver relays to `A` as a `K`, and await
    /// `<A as Replies<K>>::Reply`.
    fn relay<K, P>(&mut self, mail: P) -> Call<A, K, P>
    where
        A: Singleton + CallerAddressable + Replies<K> + Unpin + 'static,
        K: ActorMail + Send + Unpin + 'static,
        P: ActorMail + Send + Unpin + 'static,
    {
        Call::<A, K, P> { env: self.env, api: self.api, mail: Some(mail), _target: PhantomData }
    }
}

/// Sampled HTTP API: captures a call to [`aether_http::HttpCapability`].
pub struct Http(Binding<aether_http::HttpCapability>);

impl InjectedApi for Http {
    type Target = aether_http::HttpCapability;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self(Binding::new(env, ProgramApi::Http))
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

/// Sampled process API: captures a call to [`aether_process::ProcessCapability`].
pub struct Process(Binding<aether_process::ProcessCapability>);

impl InjectedApi for Process {
    type Target = aether_process::ProcessCapability;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self(Binding::new(env, ProgramApi::Process))
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

/// Sampled workspace API: runs steps over a stored tree through
/// [`aether_bloomery_workspace::WorkspaceCapability`] (ADR-0237 decision 7).
pub struct Workspace(Binding<aether_bloomery_workspace::WorkspaceCapability>);

impl InjectedApi for Workspace {
    type Target = aether_bloomery_workspace::WorkspaceCapability;
    const SAMPLED: bool = true;

    fn from_env(env: &mut Env<Async>) -> Self {
        Self(Binding::new(env, ProgramApi::Workspace))
    }
}

impl Workspace {
    /// Await the outcome of `run`, or the workspace's refusal. A non-zero exit is an outcome.
    ///
    /// The request names no storage: the driver that relays it runs it over
    /// the journal of the program's own unit (ADR-0240 I-5).
    ///
    /// An exhausted allotment or an executor failure ends the invocation instead of
    /// resolving; the program never observes either.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputDecode`] when the reply is not a `RunResult`; the pump's
    /// refusal when the call could not be sent.
    pub fn run(
        &mut self,
        run: aether_bloomery_workspace::RunRequest,
    ) -> impl Future<
        Output = Result<Result<aether_bloomery_workspace::Outcome, aether_bloomery_workspace::Refusal>, Refusal>,
    > + Send
    + 'static {
        RunCall { call: self.0.relay::<aether_bloomery_workspace::Run, _>(run) }
    }
}

/// [`Workspace::run`]'s future: the run's outcome or refusal resolves; an
/// exhausted allotment or an executor failure ends the invocation through
/// [`Env::end`] and never resolves.
struct RunCall {
    call: Call<
        aether_bloomery_workspace::WorkspaceCapability,
        aether_bloomery_workspace::Run,
        aether_bloomery_workspace::RunRequest,
    >,
}

impl Future for RunCall {
    type Output = Result<Result<aether_bloomery_workspace::Outcome, aether_bloomery_workspace::Refusal>, Refusal>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let env = this.call.env;
        let fault = match Pin::new(&mut this.call).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(refusal)) => return Poll::Ready(Err(refusal)),
            Poll::Ready(Ok(aether_bloomery_workspace::RunResult::Ok(outcome))) => return Poll::Ready(Ok(Ok(outcome))),
            Poll::Ready(Ok(aether_bloomery_workspace::RunResult::Err(error))) => match error {
                aether_bloomery_workspace::RunError::Refused(refusal) => return Poll::Ready(Ok(Err(refusal))),
                aether_bloomery_workspace::RunError::Exhausted(aether_bloomery_workspace::Resource::Time) => {
                    ExecutorFault::TimedOut
                }
                aether_bloomery_workspace::RunError::Exhausted(aether_bloomery_workspace::Resource::Memory) => {
                    ExecutorFault::ResourceExhausted
                }
                aether_bloomery_workspace::RunError::Failed { detail } => ExecutorFault::Failed { reason: detail },
            },
        };
        env.end(fault);
        Poll::Pending
    }
}

/// One captured call: the payload `P` the driver relays to `A` as its row
/// `K`, answered by `<A as Replies<K>>::Reply`.
struct Call<A, K, P> {
    env: Env<Async>,
    api: ProgramApi,
    mail: Option<P>,
    _target: PhantomData<fn() -> (A, K)>,
}

impl<A, K, P> Future for Call<A, K, P>
where
    A: Singleton + CallerAddressable + Replies<K> + Unpin + 'static,
    K: ActorMail + Send + Unpin + 'static,
    P: ActorMail + Send + Unpin + 'static,
{
    type Output = Result<<A as Replies<K>>::Reply, Refusal>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.env.take_call_reply() {
            return Poll::Ready(decode_call_reply::<<A as Replies<K>>::Reply>(result));
        }
        if let Some(mail) = this.mail.take() {
            this.env.request_send(PendingCall::new::<A, K, P>(this.api, &mail));
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
    /// Key each member by the digest its sender claims. No byte is read:
    /// a typed read verifies the member it loads.
    pub(crate) fn from_closure(closure: Vec<ClosureArtifact>) -> Self {
        let mut artifacts = BTreeMap::new();
        for artifact in closure {
            artifacts.insert(artifact.claimed().unverified(), artifact);
        }
        Self {
            inner: Box::new(RefCell::new(Inner {
                closure: artifacts,
                staged: Vec::new(),
                pending: None,
                call_reply: None,
                terminal: BTreeMap::new(),
                ended: None,
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
        // SAFETY: `inner` is the address of the `RefCell` inside an [`EnvOwner`]
        // that outlives every handle cloned from it — the local in a sync
        // `run`, or the `AsyncSession` that owns both the box and the boxed future.
        unsafe { &*(self.inner as *const RefCell<Inner>) }
    }

    /// Load `r` from the injected closure.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent (or a journal fetch reported missing).
    /// [`Refusal::InputDecode`] when the kind prefix differs, the bytes do not hash to the
    /// digest, or the payload does not decode.
    pub fn injected<K: Storage>(&self, r: Ref<K>) -> Result<K, Refusal> {
        self.decode_injected(r.digest())
    }

    /// Load UTF-8 text `r` from the injected closure.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent (or a journal fetch reported missing).
    /// [`Refusal::InputDecode`] when the kind prefix differs, the bytes do not hash to the
    /// digest, or the payload is not UTF-8.
    pub fn injected_text(&self, r: Ref<Utf8Text>) -> Result<String, Refusal> {
        String::from_utf8(self.load_injected(r.digest(), Utf8Text::ID)?).map_err(|_| Refusal::InputDecode)
    }

    /// Stage `payload` as [`OpaqueBytes`]. Identical payloads yield one artifact.
    pub fn stage_bytes(&mut self, payload: &[u8]) -> Ref<OpaqueBytes> {
        Ref::from_digest(self.record(EncodedArtifact::opaque_bytes(payload)))
    }

    /// Stage UTF-8 `text` as [`Utf8Text`]. Identical payloads yield one artifact.
    pub fn stage_text(&mut self, text: &str) -> Ref<Utf8Text> {
        Ref::from_digest(self.record(EncodedArtifact::text(text)))
    }

    /// Stage `payload`, already encoded, under `kind`, for a program that
    /// links no Rust type of `kind`. Identical payloads yield one artifact.
    ///
    /// The artifact supplies no citations: a payload that cites artifacts
    /// stages as if it cited none (see [`EncodedArtifact::uncited`]).
    pub fn stage_payload(&mut self, kind: KindId, payload: &[u8]) -> ErasedRef {
        ErasedRef::new(kind, self.record(EncodedArtifact::uncited(kind, payload)))
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
        K::decode_storage(&self.load_injected(digest, K::ID)?).map(|data| data.value).map_err(|_| Refusal::InputDecode)
    }

    /// The payload of the member at `digest`, loaded through
    /// [`ClosureArtifact::load`]: every byte is hashed against `digest`
    /// before any is returned, so a mismatch refuses before a decode sees it.
    fn load_injected(&self, digest: Digest, kind: KindId) -> Result<Vec<u8>, Refusal> {
        let inner = self.cell().borrow();
        if let Some(refusal) = inner.terminal.get(&digest) {
            return Err(refusal.clone());
        }
        let artifact = inner.closure.get(&digest).ok_or(Refusal::InputMissing)?;
        if artifact.kind() != kind {
            return Err(Refusal::InputDecode);
        }
        Ok(artifact.load(digest)?)
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

    /// Load the payload `r` cites, unprefixed and undecoded, from the injected
    /// map, or fetch it from the journal on a miss: [`Self::read`] for a
    /// program that links no Rust type of the cited kind.
    ///
    /// # Errors
    ///
    /// [`Refusal::InputMissing`] when the digest is absent after the journal replies missing.
    /// [`Refusal::InputDecode`] when the stored kind is not `r`'s, the bytes do not hash to
    /// the digest, or the journal reports a backend failure.
    pub async fn read_payload(&mut self, r: ErasedRef) -> Result<Vec<u8>, Refusal> {
        ReadPayload { env: *self, cited: r, requested: false }.await
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

    /// End the invocation with `fault`: the session finishes `Invoked::Faulted`
    /// on this poll, whatever the program's future returned. Only an executor
    /// binding's future calls it.
    pub(crate) fn end(self, fault: ExecutorFault) {
        self.cell().borrow_mut().ended = Some(fault);
    }

    pub(crate) fn take_ended(self) -> Option<ExecutorFault> {
        self.cell().borrow_mut().ended.take()
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
            ReadArtifactResult::Found { artifact } => {
                // `AsyncSession::fulfill` matched the claim to the requested
                // digest; a read verifies the bytes against that digest.
                let digest = artifact.claimed().unverified();
                inner.terminal.remove(&digest);
                inner.closure.insert(digest, artifact);
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

struct ReadPayload {
    env: Env<Async>,
    cited: ErasedRef,
    requested: bool,
}

impl Future for ReadPayload {
    type Output = Result<Vec<u8>, Refusal>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let (digest, kind) = (this.cited.digest(), this.cited.kind());
        match this.env.load_injected(digest, kind) {
            Ok(payload) => Poll::Ready(Ok(payload)),
            Err(refusal) if this.env.terminal(digest) => Poll::Ready(Err(refusal)),
            Err(Refusal::InputMissing) if !this.requested => {
                this.env.request(digest, kind);
                this.requested = true;
                Poll::Pending
            }
            Err(Refusal::InputMissing) => Poll::Pending,
            Err(refusal) => Poll::Ready(Err(refusal)),
        }
    }
}
