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
  "rig": "rig",
  "palette": "palette.txt"
}
```

`labels`, `rig` and `palette` may be empty. The charted face needs the material labels to
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

## The painter's box

`palette` points at the box a subject is painted out of. Empty gets the
canonical box — the pigments the crate was tuned on — so the mascot loads
exactly as before.

The box matters because pigments are per subject. A field cell names its class
by *position*, so the same byte means hair under one vocabulary and grass under
another, and a scene's rock and timber are not her indigo and rose. The field is
therefore validated against whichever box is going to paint it: a cell running
past that box's vocabulary is refused with the cell and the class named, rather
than developing as a hole in the sheet.

The format is line-oriented records, comments after `#`:

```text
classes  <name>...                                       the vocabulary, class id = index + 1
material <class> <pigment> <load> <gran> <floor>         one entry, in mixing order
lit      <class> <tone>                                  the entry names its own fully-lit tone
small    <class>                                         a region too small to loosen
air      <class> <halo> <across> <down> <pigment> <cap>  what it leaves in the air past its edge
remap    <child> <parent>                                the child takes the parent's wash
reserve  <class>                                         paper, left bare
```

Every class the vocabulary declares must be painted, remapped or reserved
exactly once — a class an author simply forgot is refused rather than silently
left unpainted. A vocabulary carries at most eight classes, which is what the
bake's per-class indicator lanes hold. `material iris` names the one
meta-material: the chart supplies its coverage rather than the field, so it sits
outside the vocabulary.

Mixing order is authored and load-bearing. Compositing commutes, so it carries
no meaning for the colour — but each wash's accidents are rolled from one shared
stream material by material, so two boxes naming the same entries in different
orders are two different paintings.

A few marks belong to a *named* material rather than to any material, and are
resolved by name against the active vocabulary: only `hair` throws drops, takes
the violet glaze and is brushed down its own locks; only `dress` wears less
water and gives up its far edge on the shorter run; only `skin` flushes. A box
carrying no such class simply never earns those marks. The same rule governs the
face: the chart, the iris and the blush activate only when `lips`, `brow` and
`eye` are all in the vocabulary, so a subject without a face plants nothing.

`aether-puppet`'s own `Palette::canonical` box, written in this format, is the
crate's `CANONICAL_TEXT` constant — the worked example to copy from.

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
