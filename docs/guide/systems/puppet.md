# Puppet controls

`aether-puppet` is the live pen-plotter mascot renderer. Its explicit
`aether.puppet` export loads a subject, owns the camera that frames it, and
accepts chart and rig state as mail. A normally loaded instance is addressed at:

```text
aether.component/aether.embedded:aether.puppet
```

The module is defaultless because it also exports the idle and turntable
motors. Select `aether.puppet` when loading it.

## Load a subject

Send `aether.puppet.load` with paths in one of the substrate's file namespaces:

```json
{
  "namespace": "assets",
  "path": "subject.obj",
  "labels": "labels.npy",
  "material_field_padding": 0.12,
  "rig": "rig"
}
```

`labels` and `rig` may be empty. The charted face needs the material labels to
measure its anchors. The optional rig directory contains `weights.npy` and
`rig.txt`. Those disk formats stay compatible with the external bake pipeline;
the puppet decodes them once into the declared `aether.puppet.rig_weights` and
`aether.puppet.rig_descriptor` kinds. A descriptor with malformed or unknown
records, or a weight row that cannot fit the declared four-influence vertex
format, is refused instead of being defaulted or truncated.

`material_field_padding` is the fraction of the mesh's longest axis that the
material-field baker added on each side; `0.12` is the canonical asset's value
and remains `Load::default()`'s value. The decoded
`aether.puppet.material_field` kind declares its dimensions, byte cells,
world-space origin and spacing, and ordered class vocabulary in memory; the
on-disk asset remains a NumPy 1.0 `|u1`, C-order cube.

## Drive the chart

The four controls compose over one chart state. They do not reload or
re-extract the subject.

### Expression

`aether.puppet.expression` selects the mouth, brows, and eye aperture together:

```json
{ "name": "happy" }
```

Names are `rest`, `happy`, `grin`, `angry`, `surprised`, `smug`, `sad`, and
`speaking`. Changing expression preserves the current gaze. It does replace a
mouth chosen by earlier viseme mail with the expression's own mouth.

### Gaze

`aether.puppet.gaze` moves both eyes together and makes both lids follow the
vertical direction:

```json
{ "x": 0.75, "y": -0.5 }
```

Both axes clamp to `[-1, 1]`. Positive `x` is toward her left and positive `y`
is up. Non-finite values are ignored. Gaze is chart state, not a camera or bone.

### Viseme

`aether.puppet.viseme` changes only the mouth:

```json
{ "name": "A" }
```

The speech sequence is `closed`, `A`, `I`, `U`, `E`, `O`, then `closed` again.
The chart's expression mouth shapes—`rest`, `smile`, `grin`, `frown`, `smirk`,
and `pout`—are also accepted. Brows, eye aperture, gaze, and eye design stay as
they were.

### Eye archetype

`aether.puppet.eye_archetype` changes only the drawn eye design:

```json
{ "name": "kitsune" }
```

Names are `kitsune`, `vulpine`, `sketch`, `cool`, `soft`, `wide`, and `mask`.
`mask` intentionally has no iris and therefore cannot show gaze.

Unknown expression, viseme, or archetype names leave the current state intact.

## Capture every control state

The ignored capture instrument drives the public wasm inbox and writes every
full-resolution frame plus four labeled contact sheets. It produces evidence
for human inspection; it does not assign a visual verdict.

Build the release component, then run:

```text
cargo xtask dist --no-bins --profile release
AETHER_CROSSFEED_DIR=/path/to/subject \
AETHER_PUPPET_CONTROL_DIR=/path/to/output \
cargo test -p aether-puppet --release --test pace_instrument \
  -- --ignored --nocapture chart_controls_capture
```

The subject directory contains `subject.obj` and `labels.npy`. The output
directory receives raw files such as `expression-happy.png` and the four sheets
`expression-sheet.png`, `gaze-sheet.png`, `viseme-sheet.png`, and
`archetype-sheet.png`.
