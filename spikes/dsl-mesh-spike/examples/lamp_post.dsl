; Example from ADR-0026 §Format, with a small extension to exercise more
; of the v1 vocabulary. Demonstrates: composition, translate, mirror,
; box, cylinder, sphere, palette indices.
(composition
  (translate (0 0 0)
    (box 0.4 0.1 0.4 :color 1))
  (translate (0 1.5 0)
    (cylinder 0.05 3 8 :color 2))
  (translate (0 3 0)
    (sphere 0.2 2 :color 3))
  (mirror x
    (translate (0.3 2.5 0)
      (cylinder 0.03 0.5 6 :color 2))))
