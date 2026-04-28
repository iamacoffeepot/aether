; Teapot-body silhouette via lathe. Profile traces the outer wall
; from base (axis) outward, up around the bulge, and back inward to
; the lid sit. The mesh is a closed surface with a tiny rim cap; spout
; and handle are out of scope until the v2 vocabulary lands torus and
; sweep-along-path (see ADR-0026's parked v2 list).
;
; Profile reads bottom→top: each pair (radius, y) is a silhouette point.
(lathe
  ((0.0  0.0)     ; centre of the base (closes the bottom via collapse)
   (0.45 0.0)     ; foot ring
   (0.5  0.05)    ; foot fillet
   (0.65 0.25)    ; lower bulge widening
   (0.7  0.5)     ; equator
   (0.65 0.75)    ; upper shoulder narrowing
   (0.45 0.95)    ; lid sit
   (0.4  1.0)     ; rim
   (0.0  1.0))    ; top centre (closes the top via collapse)
  24
  :color 0)
