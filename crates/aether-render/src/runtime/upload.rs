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

use std::collections::{HashSet, VecDeque};

/// One staged thing, across the three id spaces the renderer hands out.
/// Plain textures, texture arrays and volumes share the texture id space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Resource {
    Texture { texture_id: u32 },
    Geometry { geometry_id: u32 },
    Instances { instances_id: u32 },
}

/// What offering one piece of a resource to its registry did. A piece is
/// the smallest upload the renderer makes in one step: a whole geometry,
/// an instance buffer's staged range, a whole texture, a whole volume, or
/// one layer of a texture array with all its levels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Piece {
    /// The id names nothing live. The resource leaves the queue and
    /// costs nothing.
    Missing,
    /// The resource was already resident, because a draw got there first
    /// or a refused update left it as it was. It leaves the queue and
    /// costs nothing.
    Resident,
    /// One piece was uploaded and the resource is now resident. It leaves
    /// the queue and costs one.
    Landed,
    /// One piece was uploaded and more remain, which only a texture array
    /// answers. It stays at the front and costs one.
    More,
}

/// The staged resources waiting for the upload step, in the order each
/// was first staged.
pub(super) struct UploadQueue {
    /// The queued resources, front first.
    order: VecDeque<Resource>,
    /// The same resources, for the membership test. It holds exactly the
    /// elements of `order`.
    queued: HashSet<Resource>,
    /// How many pieces one step uploads. Fixed at boot and never zero:
    /// `RenderCapability::init` refuses a zero knob.
    pieces_per_frame: u32,
}

impl UploadQueue {
    /// An empty queue whose step uploads `pieces_per_frame` pieces.
    pub(super) fn new(pieces_per_frame: u32) -> Self {
        Self { order: VecDeque::new(), queued: HashSet::new(), pieces_per_frame }
    }

    /// Staging mail named `resource`. It joins the back of the queue
    /// unless it is already queued, in which case it keeps its place.
    pub(super) fn staged(&mut self, resource: Resource) {
        if self.queued.insert(resource) {
            self.order.push_back(resource);
        }
    }

    /// `resource` was destroyed: it is never offered again.
    pub(super) fn forget(&mut self, resource: Resource) {
        if self.queued.remove(&resource) {
            self.order.retain(|queued| *queued != resource);
        }
    }

    /// Offer pieces from the front of the queue to `upload` until the
    /// frame's allowance of uploads is spent or the queue is empty. Only
    /// an upload counts against the allowance: a resource that is missing
    /// or already resident leaves for free.
    pub(super) fn step(&mut self, mut upload: impl FnMut(Resource) -> Piece) {
        let mut uploaded = 0;
        while uploaded < self.pieces_per_frame {
            let Some(&front) = self.order.front() else {
                return;
            };

            match upload(front) {
                Piece::Missing | Piece::Resident => self.forget_front(front),
                Piece::Landed => {
                    self.forget_front(front);
                    uploaded += 1;
                }
                Piece::More => uploaded += 1,
            }
        }
    }

    /// Take `front`, the resource at the front of `order`, out of both
    /// tables.
    fn forget_front(&mut self, front: Resource) {
        self.order.pop_front();
        self.queued.remove(&front);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(geometry_id: u32) -> Resource {
        Resource::Geometry { geometry_id }
    }

    /// Run one step, answering each offer from `script` by geometry id
    /// and popping that id's next answer, and return the ids offered in
    /// order.
    fn offered(queue: &mut UploadQueue, script: &mut [(u32, Vec<Piece>)]) -> Vec<u32> {
        let mut offers = Vec::new();
        queue.step(|resource| {
            let Resource::Geometry { geometry_id } = resource else {
                panic!("the tests queue only geometries");
            };
            offers.push(geometry_id);
            let (_, answers) =
                script.iter_mut().find(|(scripted, _)| *scripted == geometry_id).expect("every queued id is scripted");

            answers.remove(0)
        });

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
        let mut script = [
            (0, vec![Piece::Missing]),
            (1, vec![Piece::Resident]),
            (2, vec![Piece::Landed]),
            (3, vec![Piece::Landed]),
            (4, vec![Piece::Landed]),
        ];

        assert_eq!(offered(&mut queue, &mut script), [0, 1, 2, 3], "two free departures, two uploads, then stop");
        assert_eq!(offered(&mut queue, &mut script), [4], "the fifth waits for the next step and is not lost");

        let mut queue = UploadQueue::new(3);
        queue.staged(geometry(7));
        queue.staged(geometry(8));
        let mut script = [(7, vec![Piece::More, Piece::More, Piece::Landed]), (8, vec![Piece::Landed])];

        assert_eq!(offered(&mut queue, &mut script), [7, 7, 7], "each piece of one resource counts against the step");
        assert_eq!(offered(&mut queue, &mut script), [8], "a landed resource leaves and the next takes the front");
        assert_eq!(offered(&mut queue, &mut script), [0u32; 0], "an empty queue offers nothing");
    }

    /// Stage A, B, A, then destroy B: the step offers A once and never B.
    /// The named bugs: a second staging mail queueing the resource again
    /// or moving it behind later arrivals, and a destroyed id left in the
    /// order to be offered.
    #[test]
    fn a_second_staging_keeps_the_first_position_and_a_forgotten_resource_is_never_offered() {
        let mut queue = UploadQueue::new(8);
        queue.staged(geometry(1));
        queue.staged(geometry(2));
        queue.staged(geometry(3));
        queue.staged(geometry(1));
        queue.forget(geometry(2));
        let mut script = [(1, vec![Piece::Landed]), (3, vec![Piece::Landed])];

        assert_eq!(offered(&mut queue, &mut script), [1, 3], "A keeps the front, B is gone, each is offered once");
        assert_eq!(offered(&mut queue, &mut script), [0u32; 0], "nothing is left queued");
    }
}
