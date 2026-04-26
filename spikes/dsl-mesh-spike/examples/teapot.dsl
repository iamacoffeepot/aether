; Full teapot composition.
;
; Three pieces, all centered around (0, 0, 0):
;   - body+lid+knob as one continuous lathe (axially symmetric solid)
;   - handle: torus rotated 90° around Z so its donut hole faces sideways,
;     translated to the +x shoulder
;   - spout: 8-sided tube swept along a path arcing from the -x shoulder
;     outward and upward
;
; Color indices are placeholder palette refs (per ADR-0026's
; palette-indexed model); the spike ignores them and the static-mesh
; viewer paints everything default-blue today.
(composition
  ; --- BODY + LID + KNOB ---
  ; Profile traces base centre → foot → bulge → equator → shoulder →
  ; rim → over the lid → knob top.
  (lathe
    ((0.00 0.00)    ; base centre, closes bottom via axis collapse
     (0.45 0.00)    ; foot ring
     (0.50 0.05)    ; foot fillet
     (0.65 0.25)    ; lower bulge
     (0.70 0.50)    ; equator
     (0.65 0.75)    ; shoulder narrowing
     (0.45 0.95)    ; lid sit
     (0.40 1.00)    ; rim outer
     (0.32 1.00)    ; rim inner
     (0.34 1.06)    ; lid bulge
     (0.18 1.16)    ; lid taper
     (0.07 1.20)    ; knob base shoulder
     (0.05 1.25)    ; knob stem
     (0.00 1.30))   ; knob top, closes via axis collapse
    20 :color 0)

  ; --- HANDLE ---
  ; Half-heart silhouette at full size: the centerline traces the right
  ; half of a heart in the XY plane and the wire is a 0.036-radius
  ; 8-sided tube. Closed loop attaches at upper and lower waypoints
  ; inside the body wall (overlap-by-transform per ADR-0026). All Z=0
  ; so the handle's plane stands perpendicular to the spout-handle line
  ; — hole faces ±Z. ~80% of the handle's height sits above the body
  ; midline (y=0.5): top at 0.97, bottom-tip at 0.38.
  (sweep
    ((0.036 0.000) (0.0254 0.0254) (0.000 0.036) (-0.0254 0.0254)
     (-0.036 0.000) (-0.0254 -0.0254) (0.000 -0.036) (0.0254 -0.0254))
    ((0.35 0.92 0)    ; upper attach (deep inside body wall)
     (0.80 0.97 0)    ; rising over the lobe top
     (0.95 0.94 0)    ; lobe peak
     (1.05 0.83 0)    ; upper-right curving down
     (1.08 0.70 0)    ; outer max upper
     (1.05 0.55 0)    ; outer max lower
     (0.97 0.45 0)    ; curving back inward
     (0.85 0.40 0)    ; approaching tip
     (0.73 0.38 0)    ; bottom-tip (heart point) — at 80% line
     (0.60 0.45 0)    ; rising back toward body
     (0.45 0.62 0))   ; lower attach (deep inside body wall)
    :color 0)

  ; --- SPOUT ---
  ; 8-sided unit-radius cross-section, scaled per-waypoint via :scales
  ; for a base-wide / tip-narrow taper. Path arcs out and up in 6
  ; waypoints for a smooth curve. Base waypoint sits inside the body
  ; wall (overlap-by-transform) so the spout reads as attached.
  (sweep
    ((0.080 0.000) (0.057 0.057) (0.000 0.080) (-0.057 0.057)
     (-0.080 0.000) (-0.057 -0.057) (0.000 -0.080) (0.057 -0.057))
    ((-0.50 0.55 0)
     (-0.68 0.62 0)
     (-0.83 0.74 0)
     (-0.93 0.88 0)
     (-0.97 1.00 0))
    :scales (1.6 1.3 1.0 0.75 0.55)
    :color 0))
