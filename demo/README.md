# The puppet turntable demo

The Utah teapot, drawn as pen-plotter line art, turning on a slow
revolve. It is the shortest path from a clone to something on screen: no
operator, no MCP session, no mail sent by hand.

## Run it from the checkout

Two commands. The first cross-builds the component wasm; the second boots
the desktop chassis against the checked-in boot manifest.

```sh
cargo xtask build-wasm
cargo run -p aether-chassis-desktop --bin aether-desktop -- \
  --boot-manifest demo/puppet-turntable.boot.json \
  --assets-dir crates/aether-mesh/examples
```

Run both from the repository root: a boot manifest's paths are resolved
as-is, so `dist/components/aether_puppet.wasm` and `demo/puppet.json` are
read relative to the process working directory.

## Build the shippable package

```sh
cargo xtask package --profile release \
  --spec demo/puppet-turntable.json \
  --assets crates/aether-mesh/examples
```

The depot lands in `target/package/` (`--out` moves it): the desktop
chassis binary, the workspace licenses, `pack/manifest`, the component and
config objects under `pack/objects/`, and the asset tree verbatim under
`pack/assets/`. Run `target/package/aether-desktop` — with no flags at all,
because everything the demo needs is inside the depot. That directory is
the thing you would upload.

## What appears

A line-art Utah teapot, revolving once every twelve seconds. Drag with
the mouse to orbit it yourself and the wheel to dolly in and out — the
turntable restates its own pose each frame, so the drag reads as a nudge
against a moving subject rather than a handover.

The packaged window is titled `aether`; the developer run gets the chassis's
own default title, because a boot manifest deliberately drops its chassis
settings (a hub-spawned engine is runtime-managed, not a product) while a
depot manifest carries them.

## What the four files are

| File | What it is |
|---|---|
| `puppet-turntable.json` | the depot spec `cargo xtask package --spec` reads |
| `puppet-turntable.boot.json` | the JSON boot manifest the dev run reads |
| `puppet.json` | the puppet's init-config: which subject to load |
| `turntable.json` | the turntable's init-config: rate and framing |

Both manifests name the same two configs and the same two exports from one
wasm — `aether.puppet` draws, `aether.puppet-turntable` turns it — so the
packaged product and the developer run are the same composition reached two
ways. They differ only in how paths anchor: a **depot spec** resolves
relative paths against the spec file's own directory, a **boot manifest**
resolves them as-is against the working directory.

Each config is JSON rather than a blob of init-config bytes, and is encoded
against the `Config` schema the component's own wasm declares. A field the
component does not have fails the build with the file and the field named,
rather than arriving as a decode error inside the guest.

## Changing the subject

The subject is `assets/utah_teapot.obj` — Martin Newell's 1975 teapot,
tessellated from its 32 bicubic Bézier patches. Wavefront OBJ is text, so
it lives in the tree (`crates/aether-mesh/examples/`) rather than as a
committed binary, and the demo needs no build step to have something to
draw. Point `puppet.json`'s `subject.path` at any other `.dsl` or `.obj`
under the asset root and rerun. `teapot.dsl`, `lamp_post.dsl` and
`box.dsl` sit beside it.

The checked-in file is a generated artefact, at ten subdivisions along
each patch edge — 3241 vertices, 6320 triangles. To rewrite it, or to
write a denser one somewhere else:

```sh
cargo run -p aether-mesh --example utah_teapot -- 10 \
  > crates/aether-mesh/examples/utah_teapot.obj
```
