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

The asset includes a welded continuous facial surface with modeled eye sockets,
cheeks, nose, mouth, jaw, and chin; separate procedural eyes, lids, brows, ears,
and neck; authored materials; and ten named facial morph targets. Later spike
work can load the same file through Aether and connect its morph targets to
creator controls.
