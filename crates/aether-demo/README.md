# The demo

A teapot authored in the mesh DSL, drawn in the desktop chassis and framed
by a camera you can steer from the keyboard. It is the shortest path from a
clone to something on screen: no operator, no MCP session, no mail sent by
hand.

## Run it from the checkout

Two commands. The first cross-builds the component wasm; the second boots the
desktop chassis against the checked-in boot manifest.

```sh
cargo xtask build-wasm
cargo run -p aether-chassis-desktop --bin aether-desktop -- \
  --boot-manifest crates/aether-demo/demo.boot.json \
  --assets-dir crates/aether-mesh/examples
```

Run both from the repository root: a boot manifest's paths are resolved as-is,
so `dist/components/aether_kit.wasm` and `crates/aether-demo/controller.json`
are read relative to the process working directory.

## Build the shippable package

```sh
cargo xtask package --profile release \
  --spec crates/aether-demo/demo.json \
  --assets crates/aether-mesh/examples
```

The depot lands in `target/package/` (`--out` moves it): the desktop chassis
binary, the workspace licenses, `pack/manifest`, the component and config
objects under `pack/objects/`, and the asset tree verbatim under
`pack/assets/`. Run `target/package/aether-desktop` with no flags at all,
because everything the demo needs is inside the depot. That directory is the
thing you would upload.

## What appears

`teapot.dsl` from `crates/aether-mesh/examples/`, seen from a little above
and to one side. The keys steer the camera:

| Keys | Move |
|---|---|
| W / A / S / D | pan the orbit target across the ground |
| ← / → | yaw around the target |
| ↑ / ↓ | pitch |
| Z / X | zoom in / out |

The packaged window is titled `aether`; the developer run gets the chassis's
own default title, because a boot manifest deliberately drops its chassis
settings (a hub-spawned engine is runtime-managed, not a product) while a
depot manifest carries them.

## What the files are

| File | What it is |
|---|---|
| `demo.json` | the depot spec `cargo xtask package --spec` reads |
| `demo.boot.json` | the JSON boot manifest the dev run reads |
| `controller.json` | the camera controller's init-config: rates, clamps, and the `seed` pose that frames the subject |
| `src/lib.rs` | the `aether.demo` component |

Both manifests list the same four components in the same order: the kit's
`aether.kit.camera`, `aether.kit.camera-controller` (with `controller.json`)
and `aether.kit.mesh`, then `aether.demo`. The packaged product and the
developer run are the same composition reached two ways. They differ only in
how paths anchor: a **depot spec** resolves relative paths against the spec
file's own directory, a **boot manifest** resolves them as-is against the
working directory.

The mesh viewer draws nothing until something sends it `aether.kit.mesh.load`,
and a boot list sends no mail. `aether.demo` is that something: loaded last, it
sends the viewer the same load an operator would, from `wire`, and logs the
viewer's reply (`subject loaded`, or the error). It is ordinary mail from an
ordinary component; the viewer has no other way to load a mesh.

Every entry keeps its default name, with no `name` field. The controller
declares the camera a dependency, the viewer does too, and `aether.demo`
declares the viewer. A declared dependency is found by the actor's namespace,
and the component host refuses a load whose dependency is not live, so a
renamed entry, or one moved ahead of what it depends on, fails the boot rather
than drawing nothing.

`controller.json` is JSON rather than a blob of init-config bytes, and is
encoded against the `Config` schema the kit's own wasm declares. A field the
controller does not have fails the build with the file and the field named,
rather than arriving as a decode error inside the guest.

## Changing the subject

The subject is two consts at the top of `src/lib.rs`. Point `SUBJECT_PATH` at
any other `.dsl` or `.obj` under the asset root, then rebuild the wasm and
rerun. A different subject likely wants a different `seed` in
`controller.json`: `target` is the point the camera looks at, `distance` how
far back it sits, and a negative `pitch` looks down on it. Nothing else names
the subject except the coverage band in `tests/scenario.rs`, which is tuned to
how much of the frame the framed subject fills.
