# Kitsune ear extraction — stage 1 notes

Source: `research/tessera/data/cache/144_material_labels_256.npy`
Tool: `spikes/warp-ears/tools/` (`cargo run --release -- <npy> {analyze|sweep|project|extract}`)

## Source file, as parsed rather than assumed

The npy v1.0 header reads `{'descr': '|u1', 'fortran_order': False, 'shape': (256, 256, 256), }`
— uint8, C order, 128-byte header, 16 777 216 payload bytes. The tool parses the
header dict and rejects a Fortran-order or wider-dtype file rather than
silently reading a transposed volume.

## Class mapping

Taken from `research/tessera/plot/src/labels.rs:21-37`, which documents the
producer's rule directly: `0` unlabelled, then `index + 1` into the material
list. Confirmed against the producing spike `research/tessera/spikes/176_pixal3d_kitsune_ink.py:65`
(`INNER_EAR = 4`).

| int | class | voxels in the 256³ field | share |
|----:|-------|-------------------------:|------:|
| 0 | UNLABELLED | 16 351 060 | 97.4599% |
| 1 | SKIN | 41 193 | 0.2455% |
| 2 | DRESS | 120 589 | 0.7188% |
| 3 | HAIR | 256 508 | 1.5289% |
| 4 | INNER_EAR | 2 477 | 0.0148% |
| 5 | TUFT | 3 891 | 0.0232% |
| 6 | LIPS | 293 | 0.0017% |
| 7 | BROW | 743 | 0.0044% |
| 8 | EYE | 462 | 0.0028% |

Total occupied (nonzero): 426 156, 2.5401% of the volume. Classes 9-11
(FEATHER, FEATHER_TIP, TRIM) are defined globally but unused by this
character — the vocabulary is shared across subjects and a class a character
lacks simply goes unseeded.

**The field is a surface shell, not a solid.** 2.5% occupancy over a
character that fills most of its bounding cube can only be a labelled skin
around the marched isosurface. Everything downstream had to account for that.

## Axis convention

**npy axis 0 = world X = left-right, +X is HER LEFT.
npy axis 1 = world Y = UP.
npy axis 2 = world Z = FORWARD, the direction she faces.
Right-handed, Y-up.**

Two independent lines of evidence agree.

*From the producer.* `research/tessera/tessera/volume.py:72-74` builds the
lattice as `np.meshgrid(*[linspace(lo[i], hi[i], n) for i in range(3)], indexing="ij")`
over axes ordered `[x, y, z]`, so flat row `(i*n + j)*n + k` holds world
`(x[i], y[j], z[k])`. Nothing in the write path (`flood` → `prune` → `np.save`)
transposes, swaps, or flips. Consumers index it the same way: `plot/src/labels.rs:91`
reads `cells[(ix * n + iy) * n + iz]` with `ix` derived from `p.x`.
`spikes/144_drawn_face.py:305-311` measures "top of the lips" and the eye
floor from component **1** and the nose midline from component **0**, and
takes the frontmost skin cell as `argmax(world[:, 2])`. `spikes/143_eye_chart.py:138-141`
states the handedness outright — larger x is the viewer's right at azimuth 0
and therefore *her left*. `plot/src/camera.rs:5` is "right-handed, Y up".

*From the data itself*, which is what I checked first and would have trusted
over the code had they disagreed. Occupied extents are axis 0 `[58,197]` span
140, axis 1 `[23,232]` span 210, axis 2 `[65,190]` span 126. The longest axis
of an upright character is its up axis, so axis 1 is up. The two INNER_EAR
clusters sit at axis 0 ≈ 100 and ≈ 155 — symmetric about 127.5, the exact
midpoint of the axis 0 occupied extent — so axis 0 is the left-right axis and
the character is modelled symmetric about it. Both ear clusters sit at axis 1
186-227, the top of the up range, which is where ears belong. Axis 2 is what
remains, and the face features (BROW, EYE, LIPS at axis 1 ≈ 170-177) sit at
high axis 2, confirming +Z is the facing direction.

The chosen ear is centred at axis 0 ≈ 155 > 127.5, so it is **her left ear**.

## Segmentation

### Which ear

26-connected components over INNER_EAR alone:

| # | voxels | axis0 | axis1 | axis2 | centroid |
|--:|-------:|-------|-------|-------|----------|
| 0 | 1102 | [146,161] | [190,226] | [131,142] | (155.1, 211.2, 136.7) |
| 1 | 989 | [ 94,110] | [204,227] | [131,143] | (100.4, 216.6, 136.9) |
| 2 | 386 | [ 93,104] | [186,203] | [137,144] | (97.8, 193.5, 139.9) |

Her left ear (#0) is one clean component covering the full 190-226 vertical
span of the cavity. Her right ear is the same surface broken into #1 (upper)
and #2 (lower) by a gap in the labelling around axis 1 ≈ 203. **Ear #0 was
chosen** on exactly that basis — it segments without having to decide whether
two components are one ear.

### Where the ear ends and the head begins

No label separates them: the ear shell is continuous with the skull shell, and
both are HAIR. A cut has to supply the boundary.

Sweeping a horizontal cut down the up axis and asking whether the seed ear is
still its own connected component gives a sharp answer for the upper ear: the
two ears are separate down to axis 1 = 212 and merge at 210, where the crown
of the head bridges between them. So `CROWN = 212` is the level above which
the ear is unambiguously its own object.

But a flat cut at 212 is wrong on its own, and the ASCII projections
(`project` mode) show why. The ear's INNER_EAR cavity reaches down to axis 1 =
190, twenty-two levels below the crown, because the ear attaches to a rounded
skull: the base contour is an ellipse on a curved surface, so its outer edge
dips well below the crown line. Cutting at 212 yields a 21-voxel stub, not an
ear. The projections also show that the wide mass below axis 1 ≈ 190 is the
hair volume, not skull — so no sphere fit or flat plane recovers the base
either.

The segmentation actually used is three unions:

1. **The labelled ear surface** — the INNER_EAR/TUFT connected component
   containing the seed. 1102 INNER_EAR voxels grow to 2872 once the ear tuft
   (the fur inside the cavity, class 5) is included. Taking the component that
   *touches this ear* rather than all TUFT keeps the other ear out.
2. **The shell backing it** — every occupied voxel within Chebyshev distance
   3 of that surface (+4107 voxels). This is what supplies the ear's outer
   shell where it dips below the crown and nothing labels it. It is bounded by
   construction, so it cannot swallow the hair mass.
3. **The whole protrusion above the crown** — the connected component of the
   occupied set restricted to axis 1 ≥ 212 that contains the ear (3772 voxels,
   636 of them new).

The union is then reduced to its largest seed-bearing connected component;
here that was all 7615 voxels, so the dilation stranded nothing.

The `SHELL_REACH = 3` dilation is the one genuinely tunable number in the
pipeline. It was chosen because it produces a body that is already essentially
solid — 7615 voxels carrying only 5496 exposed quads is 0.72 exposed faces per
voxel, where a hollow one-voxel shell would carry nearly 4 — which is what a
skinning/warp demo wants. A smaller reach leaves a hollow dish; a larger one
starts bulging the base into the skull.

### Solidification

The cropped box is flooded from its exterior over empty cells (6-connectivity,
one voxel of padding), and anything the exterior cannot reach is interior and
gets filled with its nearest label. The min-up face is deliberately *not* an
exterior seed, since the base cut leaves the cone open at the bottom and a
flood entering there would walk up the inside and fill nothing.

This added only **10 voxels**, confirming the reach-3 body was already solid
rather than a thick shell around a cavity.

## Resolution and the quad budget

Exposed surface quads = faces between an occupied cell and empty space, the
count a voxel mesher would emit.

| stage | dims | voxels | exposed quads |
|-------|------|-------:|--------------:|
| shell, cropped | 30 × 48 × 31 | 7 615 | 5 552 |
| solidified | 30 × 48 × 31 | 7 625 | 5 496 |
| **downsampled 2×** | **15 × 24 × 16** | **1 292** | **1 354** |

Full resolution is 5 496 quads, over the ~4 000 budget, so one 2× halving was
applied. A coarse cell is occupied when **any** of its eight children is —
majority occupancy would perforate a shell — and takes the **majority label**
among its occupied children, ties broken toward the lower class id. One
halving drops well under budget (1 354); a second was not applied because it
would leave a 12-cell-tall ear, too coarse to show the difference between
linear-blend skinning and a warp field.

Per-class composition of the shipped ear: HAIR 850, TUFT 287, INNER_EAR 155
(1 292 total). No SKIN — the ear is entirely fur and cavity, which is correct
for a fox ear.

## Rig estimate

All coordinates are box-local, in units of 2 original voxels (the downsample
factor), offset from `box_origin = [136, 186, 123]`.

The long axis comes from PCA over the occupied cells (power iteration on the
covariance, sign-flipped to point up). Base and tip are the **centroids of the
extreme deciles** along that axis rather than the extreme cells, so one stray
shell voxel cannot define where the bone starts.

- base `[9.178, 2.271, 9.450]`
- tip `[9.512, 19.581, 6.512]`
- axis `[0.019, 0.986, -0.167]` — essentially straight up, tilted slightly
  backward (−Z is behind her), which is how the ear actually sits
- length `17.56` box cells = 35.1 original voxels
- second joint at 40% up: `[9.312, 9.195, 8.274]`

The **contact plane** approximates the skull surface at the base for a later
fold-to-contact pose. Its point is the base; its normal is the outward radial
direction there, computed as `normalize(base − head_centre)`. The head centre
is the centroid of the occupied set over axis 1 ∈ [185, 211] — the band that
reads as skull rather than ear or neck — with its left-right component pinned
to the model midline 127.5 rather than taken from the centroid, because the
band also catches asymmetric hair while the character is modelled symmetric.
That gives `(127.5, 198.68, 136.64)` in original coordinates and a normal of
`[0.941, -0.285, 0.184]`: mostly laterally outward (+X, her left) and slightly
downward, because the base sits on the side of the head below the crown. An
ear folding onto this plane lays sideways-and-down against the skull, which is
the pose intended.

## Ambiguities decided, and what stage 2 should know

- **The ear/head boundary is a judgement, not a label.** Nothing in the data
  marks it. The reach-3 dilation plus crown union is a defensible choice, not a
  unique one. If the demo wants a longer ear, raise `SHELL_REACH`; the base
  will thicken into the skull before the tip changes.
- **The base is tapered, not a flat cut.** The bottom of the box is the
  dilation boundary following the cavity rim, so the ear narrows toward its
  attachment instead of ending on a plane. Good for a leaf silhouette; it does
  mean there is no flat face to weld to a head mesh, and `rig.base` sits at
  j ≈ 2.27, slightly *above* the lowest occupied cell.
- **The shipped ear is solid**, not a shell. Interior cells carry the label of
  the nearest labelled cell, so interior labels are an interpolation and should
  not be read as ground truth — only the surface labelling came from the
  source field.
- **The other ear is not interchangeable with this one.** Her right ear needs
  components #1 and #2 merged before the same pipeline would work on it, and
  its cavity labelling has a gap around axis 1 ≈ 203. Mirroring this ear across
  x = 127.5 is the cheaper route to a pair.
- **`box_dims` is the shipped (downsampled) grid**, while `box_origin` is in
  original 256³ coordinates. To go back: `original = box_origin + downsample * local`.
  The last Z plane of the box is empty (occupied local extent is
  `[0,0,0]`-`[14,23,14]` within dims `15×24×16`) — an artefact of rounding the
  crop up when halving.
- **Physical scale, if stage 3 wants real units.** The lattice is a cube over
  the source mesh's longest axis plus 12% padding either side
  (`tessera/volume.py:54,66-74`, default `pad=0.12`), giving spacing
  ≈ 0.0092355 model units per original voxel — so one shipped cell is
  ≈ 0.018471 model units, and the ear's 35.1-voxel length is ≈ 0.324 model
  units against a 1.899-unit character height.
