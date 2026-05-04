; A small ghost-spirit. Tear-drop silhouette via lathe (rounded top,
; tapering wisp bottom), two small spheres for eyes, a flattened mouth
; wedge. Symmetric around the Y axis — wisp drifts straight down.
;
; Author note: this is a sketch / first pass. Saving as DSL because
; the mesh viewer draws polygon-edge wireframes for .dsl loads, which
; is closest the engine has to "just the outlines" today. Eyes and
; mouth read as floating primitives until we layer art properly.
(composition
  ; --- BODY ---
  ; Profile traces wisp-tip → narrow waist → flaring shoulder →
  ; rounded crown. 12 lathe segments so the wireframe reads cleanly
  ; from the front.
  (lathe
    ((0.00 0.00)     ; wisp tip closes via axis collapse
     (0.05 0.05)     ; just above the tip
     (0.10 0.12)     ; wisp neck
     (0.18 0.22)     ; wisp shoulder
     (0.30 0.36)     ; flaring out
     (0.42 0.52)     ; lower body
     (0.50 0.70)     ; widest point
     (0.50 0.85)     ; carrying the round
     (0.46 1.00)     ; shoulder
     (0.36 1.12)     ; crown narrowing
     (0.22 1.20)     ; top dome
     (0.10 1.25)     ; near-top
     (0.00 1.27))    ; crown apex closes via axis collapse
    12 :color 0)

  ; --- LEFT EYE ---
  ; Small sphere set into the upper body, pulled slightly forward in
  ; +Z so it floats clear of the body silhouette in 3D and reads as a
  ; distinct ring in the wireframe.
  (translate (-0.18 0.95 0.40)
    (sphere 0.06 3 :color 1))

  ; --- RIGHT EYE ---
  (translate (0.18 0.95 0.40)
    (sphere 0.06 3 :color 1))

  ; --- MOUTH ---
  ; Tiny flat cylinder turned on its side (rotate around +X by π/2 so
  ; the circular faces point ±Y) — reads as a small dash. Just below
  ; eye-line, pulled forward in +Z like the eyes.
  (translate (0 0.78 0.42)
    (rotate (1 0 0) 1.5708
      (cylinder 0.05 0.02 8 :color 2))))
