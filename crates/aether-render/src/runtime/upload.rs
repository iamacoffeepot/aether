//! The upload queue of the `aether.render` cap (ADR-0251): the staged
//! resources waiting to reach the device ahead of their first draw.
//!
//! Create, update and layer-write mail only stages bytes. Without the
//! queue the wgpu object is made by the record path the first time a
//! pass names the resource, so everything a newly drawn piece of content
//! needs is uploaded in the one frame that first draws it. The queue
//! moves that work earlier: each frame uploads a fixed number of pieces
//! from the front, so a resource that is staged and then given frames is
//! resident before anything draws it.
//!
//! The queue holds ids only. The registries stay the source of truth for
//! whether a resource is resident: the step offers the front resource to
//! its registry and is told, as a [`Piece`], what that did. A resource a
//! draw realized first, and an id that names nothing any more, leave the
//! queue at no cost, so correctness never depends on the queue.
//!
//! The queue also holds the waits of `aether.render.await_resident`
//! (ADR-0251 sections 5 and 6): the requests owed a reply once every
//! resource they named is resident. The bookkeeping is a countdown, not a
//! scan. A wait holds how many of its resources are still to be found
//! resident, and a queued resource holds the ids of the waits that named
//! it. When the step finds a resource resident, each of those waits counts
//! it, and the one that reaches zero answers itself. A wait is never
//! counted back up, so a resource updated after it was counted does not
//! reopen the wait.
//!
//! Two lanes give the order. A queued resource is in `awaited` from the
//! first time a wait names it and in `rest` otherwise, and the step
//! empties `awaited` before it takes from `rest`.
//!
//! The tables and a wait's ticket are private to this module. The runtime
//! says what happened through [`UploadQueue::staged`],
//! [`UploadQueue::wait`], [`UploadQueue::refuse`],
//! [`UploadQueue::abandon`] and [`UploadQueue::step`].

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use aether_actor::ReplyMode;
use aether_substrate::actor::native::{Held, NativeCtx};

use crate::{AwaitResidentError, AwaitResidentResult, RenderResource};

/// What offering one piece of a resource to its registry did. A piece is
/// the smallest upload the renderer makes in one step: a whole geometry,
/// an instance buffer's staged range, a whole texture, a whole volume, or
/// one layer of a texture array with all its levels. `bytes` is what the
/// resource holds on the device now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Piece {
    /// The id names nothing live. The resource leaves the queue and
    /// costs nothing.
    Missing,
    /// The resource was already resident, because a draw got there first
    /// or a refused update left it as it was. It leaves the queue and
    /// costs nothing.
    Resident { bytes: u64 },
    /// One piece was uploaded and the resource is now resident. It leaves
    /// the queue and costs one.
    Landed { bytes: u64 },
    /// One piece was uploaded and more remain, which only a texture array
    /// answers. It stays at the front and costs one.
    More,
}

/// Where a resource stands when a wait that names it arrives, as its
/// registry reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Residency {
    /// The id names nothing live in its registry.
    Unknown,
    /// Bytes are held and the device object is missing or behind them.
    Staged,
    /// The device object holds every staged byte, `bytes` of them.
    Resident { bytes: u64 },
}

impl Residency {
    /// Where a live resource stands: resident holding `bytes`, or staged.
    pub(super) const fn live(resident: bool, bytes: u64) -> Self {
        if resident {
            Self::Resident { bytes }
        } else {
            Self::Staged
        }
    }
}

/// What the handler does with an `AwaitResident`, decided before any
/// ticket is made.
pub(super) enum Arrival {
    /// The request is answered before the handler returns.
    Answered(AwaitResidentResult),
    /// The reply is owed. `staged` is the named resources that were not
    /// resident, each once and never empty, and `bytes` is the sum over
    /// the ones that were.
    Owed { staged: Vec<RenderResource>, bytes: u64 },
}

/// Names one wait in the queue's table. Ids are handed out in arrival
/// order and never reused, so an id left on a resource's list after its
/// wait was answered names nothing.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct WaitId(u64);

/// One `AwaitResident` that has not been answered.
struct Wait {
    /// How many of its resources have not yet been found resident. It
    /// only goes down.
    remaining: usize,
    /// The bytes of those found resident so far, including the ones
    /// resident when the request arrived.
    bytes: u64,
    /// The ticket that answers the sender.
    held: Held<AwaitResidentResult>,
}

impl Wait {
    /// One of the wait's resources was found resident holding `bytes`.
    /// Whether that was the last of them.
    const fn landed(&mut self, bytes: u64) -> bool {
        self.remaining -= 1;
        self.bytes += bytes;

        self.remaining == 0
    }

    /// Answer the sender that every resource it named is resident.
    fn answer_resident<M: ReplyMode, A, S>(self, ctx: &mut NativeCtx<'_, A, S, M>) {
        self.held.answer(ctx, &AwaitResidentResult::Ok { bytes: self.bytes });
    }

    /// Answer the sender that its wait ended with `error`.
    fn answer_refused<M: ReplyMode, A, S>(self, ctx: &mut NativeCtx<'_, A, S, M>, error: AwaitResidentError) {
        self.held.answer(ctx, &AwaitResidentResult::Err(error));
    }
}

/// A wait the queue has taken out of its table, and how it ended. The
/// bookkeeping hands each one to its caller to answer, so the step's loop
/// needs no ctx and the module's tests can run it.
enum Ended {
    /// Its last resource was found resident.
    Resident(Wait),
    /// The named resource was destroyed before it became resident.
    Destroyed(Wait, RenderResource),
}

impl Ended {
    /// Send the wait's one reply. `ctx` is the render actor's own.
    fn answer<M: ReplyMode, A, S>(self, ctx: &mut NativeCtx<'_, A, S, M>) {
        match self {
            Self::Resident(wait) => wait.answer_resident(ctx),
            Self::Destroyed(wait, resource) => wait.answer_refused(ctx, AwaitResidentError::Destroyed { resource }),
        }
    }
}

/// The staged resources waiting for the upload step, and the waits on
/// them.
pub(super) struct UploadQueue {
    /// The queued resources some wait has named, in the order each was
    /// first named. The step empties this lane first.
    awaited: VecDeque<RenderResource>,
    /// The queued resources no wait has named, in the order each was
    /// first staged.
    rest: VecDeque<RenderResource>,
    /// Every queued resource, with the ids of the waits that named it. A
    /// resource is in `awaited` when its list is non-empty and in `rest`
    /// when it is empty. An id on a list may name a wait already
    /// answered, which a destroy of another of its resources did.
    queued: HashMap<RenderResource, Vec<WaitId>>,
    /// The unanswered waits.
    waits: HashMap<WaitId, Wait>,
    /// The id the next wait takes.
    next_wait: u64,
    /// How many pieces one step uploads. Fixed at boot and never zero:
    /// `RenderCapability::init` refuses a zero knob.
    pieces_per_frame: u32,
}

impl UploadQueue {
    /// An empty queue whose step uploads `pieces_per_frame` pieces.
    pub(super) fn new(pieces_per_frame: u32) -> Self {
        Self {
            awaited: VecDeque::new(),
            rest: VecDeque::new(),
            queued: HashMap::new(),
            waits: HashMap::new(),
            next_wait: 0,
            pieces_per_frame,
        }
    }

    /// Staging mail named `resource`. It joins the back of `rest` unless
    /// it is already queued, in which case it keeps its lane and place.
    pub(super) fn staged(&mut self, resource: RenderResource) {
        if let Entry::Vacant(unqueued) = self.queued.entry(resource) {
            unqueued.insert(Vec::new());
            self.rest.push_back(resource);
        }
    }

    /// Take a wait whose reply is owed. `staged` is the resources it
    /// named that were not resident when it arrived, each once and at
    /// least one; `bytes` is the sum over the ones that were; `held`
    /// answers the sender.
    ///
    /// Each staged resource goes to the back of `awaited` unless a wait
    /// already put it there: one queued in `rest` moves across, and one
    /// that is staged but not queued (a device replacement queues
    /// nothing) joins, so the wait still ends. The move out of `rest` is
    /// one pass over it for the whole request.
    pub(super) fn wait(&mut self, staged: Vec<RenderResource>, bytes: u64, held: Held<AwaitResidentResult>) {
        let id = WaitId(self.next_wait);
        self.next_wait += 1;
        self.waits.insert(id, Wait { remaining: staged.len(), bytes, held });

        let mut promoted = HashSet::new();
        for resource in staged {
            let waiters = self.queued.entry(resource).or_default();
            if waiters.is_empty() {
                self.awaited.push_back(resource);
                promoted.insert(resource);
            }
            waiters.push(id);
        }
        if !promoted.is_empty() {
            self.rest.retain(|queued| !promoted.contains(queued));
        }
    }

    /// `resource` was destroyed: it is never offered again, and every
    /// unanswered wait that named it is answered `Destroyed`. The
    /// resource is found in its lane by one pass over that lane. `ctx` is
    /// the render actor's own, which answers held replies.
    pub(super) fn refuse<M: ReplyMode, A, S>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>, resource: RenderResource) {
        let waiters = self.unqueue(resource);
        self.end_destroyed(waiters, resource, &mut |ended| ended.answer(ctx));
    }

    /// The render device failed for good with `error`, so nothing more
    /// uploads: every unanswered wait is answered `DeviceUnusable`, in
    /// arrival order. The queued resources stay as they are. `ctx` is the
    /// render actor's own.
    pub(super) fn abandon<M: ReplyMode, A, S>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>, error: &str) {
        let mut waits: Vec<(WaitId, Wait)> = self.waits.drain().collect();
        waits.sort_unstable_by_key(|(id, _)| *id);

        for (_, wait) in waits {
            wait.answer_refused(ctx, AwaitResidentError::DeviceUnusable { error: error.to_owned() });
        }
    }

    /// Offer pieces to `upload` until the frame's allowance of uploads is
    /// spent or the queue is empty, taking from `awaited` before `rest`.
    /// Only an upload counts against the allowance: a resource that is
    /// missing or already resident leaves for free. A resource found
    /// resident is counted by the waits that named it, and a wait that
    /// was on its last resource is answered here. `ctx` is the render
    /// actor's own, which answers held replies.
    pub(super) fn step<M: ReplyMode, A, S>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        upload: impl FnMut(RenderResource) -> Piece,
    ) {
        self.offer(upload, |ended| ended.answer(ctx));
    }

    /// How many waits are unanswered.
    #[cfg(test)]
    pub(super) fn unanswered(&self) -> usize {
        self.waits.len()
    }

    /// [`Self::step`] without its ctx: each wait the step ends is handed
    /// to `ended`.
    fn offer(&mut self, mut upload: impl FnMut(RenderResource) -> Piece, mut ended: impl FnMut(Ended)) {
        let mut uploaded = 0;
        while uploaded < self.pieces_per_frame {
            let Some(front) = self.front() else {
                return;
            };

            match upload(front) {
                Piece::Missing => {
                    let waiters = self.unqueue_front(front);
                    self.end_destroyed(waiters, front, &mut ended);
                }
                Piece::Resident { bytes } => {
                    let waiters = self.unqueue_front(front);
                    self.count_landed(waiters, bytes, &mut ended);
                }
                Piece::Landed { bytes } => {
                    let waiters = self.unqueue_front(front);
                    self.count_landed(waiters, bytes, &mut ended);
                    uploaded += 1;
                }
                Piece::More => uploaded += 1,
            }
        }
    }

    /// The resource the next piece comes from: the front of `awaited`,
    /// or of `rest` when no awaited resource is queued. `None` when
    /// nothing is queued.
    fn front(&self) -> Option<RenderResource> {
        self.awaited.front().or_else(|| self.rest.front()).copied()
    }

    /// Take `front`, the resource [`Self::front`] answered, out of the
    /// queue, and return the ids of the waits that named it.
    ///
    /// # Panics
    /// Panics if `front` is not queued, fail-fast per ADR-0063: every
    /// resource in a lane has its entry in `queued`.
    fn unqueue_front(&mut self, front: RenderResource) -> Vec<WaitId> {
        if self.awaited.pop_front().is_none() {
            self.rest.pop_front();
        }

        self.queued.remove(&front).expect("a resource in a lane is in the queued table")
    }

    /// Take `resource` out of the queue wherever it stands, and return
    /// the ids of the waits that named it: none when it was not queued.
    /// One pass over the lane that holds it.
    fn unqueue(&mut self, resource: RenderResource) -> Vec<WaitId> {
        let Some(waiters) = self.queued.remove(&resource) else {
            return Vec::new();
        };
        let lane = if waiters.is_empty() {
            &mut self.rest
        } else {
            &mut self.awaited
        };
        lane.retain(|queued| *queued != resource);

        waiters
    }

    /// A resource `waiters` named was found resident holding `bytes`:
    /// each of them that is still unanswered counts it, and one that was
    /// on its last resource leaves the table and is handed to `ended`.
    fn count_landed(&mut self, waiters: Vec<WaitId>, bytes: u64, ended: &mut impl FnMut(Ended)) {
        for id in waiters {
            let Entry::Occupied(mut wait) = self.waits.entry(id) else {
                continue;
            };
            if wait.get_mut().landed(bytes) {
                ended(Ended::Resident(wait.remove()));
            }
        }
    }

    /// `resource`, which `waiters` named, is gone: each of them that is
    /// still unanswered leaves the table and is handed to `ended`. Its id
    /// stays on the lists of the other resources it named and is skipped
    /// there, because it no longer names a wait.
    fn end_destroyed(&mut self, waiters: Vec<WaitId>, resource: RenderResource, ended: &mut impl FnMut(Ended)) {
        waiters
            .into_iter()
            .filter_map(|id| self.waits.remove(&id))
            .map(|wait| Ended::Destroyed(wait, resource))
            .for_each(ended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(geometry_id: u32) -> RenderResource {
        RenderResource::Geometry { geometry_id }
    }

    const RESIDENT: Piece = Piece::Resident { bytes: 0 };
    const LANDED: Piece = Piece::Landed { bytes: 0 };

    /// Run one step, answering each offer from `script` by geometry id
    /// and popping that id's next answer, and return the ids offered in
    /// order. The step runs through `offer`, the loop without its ctx:
    /// no wait is held here, because a wait's ticket is made only by a
    /// live actor's ctx, and the waits are tested through the fixture in
    /// the runtime's tests.
    fn offered(queue: &mut UploadQueue, script: &mut [(u32, Vec<Piece>)]) -> Vec<u32> {
        let mut offers = Vec::new();
        let upload = |resource| {
            let RenderResource::Geometry { geometry_id } = resource else {
                panic!("the tests queue only geometries");
            };
            offers.push(geometry_id);
            let (_, answers) =
                script.iter_mut().find(|(scripted, _)| *scripted == geometry_id).expect("every queued id is scripted");

            answers.remove(0)
        };
        queue.offer(upload, |_| panic!("these tests hold no wait, so the step ends none"));

        offers
    }

    /// With the allowance at 2, a missing and a resident resource at the
    /// front leave without counting, the next two upload and the fifth is
    /// not offered; and a resource that has more pieces is offered again
    /// at the front and counts each time. The named bugs: counting a free
    /// departure against the allowance, popping a resource that has more
    /// pieces, and looping past the allowance.
    #[test]
    fn a_step_uploads_the_allowance_and_free_departures_do_not_count() {
        let mut queue = UploadQueue::new(2);
        (0..5).for_each(|geometry_id| queue.staged(geometry(geometry_id)));
        let mut script =
            [(0, vec![Piece::Missing]), (1, vec![RESIDENT]), (2, vec![LANDED]), (3, vec![LANDED]), (4, vec![LANDED])];

        assert_eq!(offered(&mut queue, &mut script), [0, 1, 2, 3], "two free departures, two uploads, then stop");
        assert_eq!(offered(&mut queue, &mut script), [4], "the fifth waits for the next step and is not lost");

        let mut queue = UploadQueue::new(3);
        queue.staged(geometry(7));
        queue.staged(geometry(8));
        let mut script = [(7, vec![Piece::More, Piece::More, LANDED]), (8, vec![LANDED])];

        assert_eq!(offered(&mut queue, &mut script), [7, 7, 7], "each piece of one resource counts against the step");
        assert_eq!(offered(&mut queue, &mut script), [8], "a landed resource leaves and the next takes the front");
        assert_eq!(offered(&mut queue, &mut script), [0u32; 0], "an empty queue offers nothing");
    }

    /// Stage A, B, A, then take B out as a destroy does: the step offers
    /// A once and never B. The named bugs: a second staging mail queueing
    /// the resource again or moving it behind later arrivals, and a
    /// destroyed id left in its lane to be offered.
    #[test]
    fn a_second_staging_keeps_the_first_position_and_a_forgotten_resource_is_never_offered() {
        let mut queue = UploadQueue::new(8);
        queue.staged(geometry(1));
        queue.staged(geometry(2));
        queue.staged(geometry(3));
        queue.staged(geometry(1));
        assert!(queue.unqueue(geometry(2)).is_empty(), "no wait named B");
        let mut script = [(1, vec![LANDED]), (3, vec![LANDED])];

        assert_eq!(offered(&mut queue, &mut script), [1, 3], "A keeps the front, B is gone, each is offered once");
        assert_eq!(offered(&mut queue, &mut script), [0u32; 0], "nothing is left queued");
    }
}
