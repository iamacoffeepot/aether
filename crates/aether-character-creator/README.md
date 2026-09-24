# Character creator spike

This spike generates an original GLB 2.0 human head from Rust source. It does
not contain or derive from a third-party mesh, scan, texture, or character
asset. The checked-in GLB is reproducible with:

```sh
cargo run -p aether-character-creator --bin generate-head
```

Render a deterministic portrait from the same generated geometry:

```sh
cargo run -p aether-character-creator --bin preview-head
```

The preview command refuses to render when the checked-in GLB differs from a
fresh generator run, keeping the image tied to the binary asset.

The first asset includes a continuous head surface, separate procedural eyes,
simple authored materials, and named facial morph targets. Later spike work can
load the same file through Aether and connect its morph targets to creator
controls.
