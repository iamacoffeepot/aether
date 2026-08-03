//! The puddle ops as authored passes (iamacoffeepot/aether#4366).
//!
//! Where the water decides the edge: the separable box blur (iterated
//! small-tap, held against the CPU running sum within a stated similarity
//! threshold rather than bit-exactly), the shrink that resamples a pour
//! about its centroid with the pre-rolled jitter, the threshold that cuts
//! the softened puddle along a window of the tide-line noise, and the rim
//! — alpha minus its own blur, noise-varied along the tide line.
