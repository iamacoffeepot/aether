# Media, interaction, and product tools

These systems turn engine state into human-visible or editable output. Native
capabilities own devices and low-level resources; `aether-kit-commons` and
`aether-kit-widget` actors compose them into reusable camera, mesh, console, and
widget behavior.

| Concern | Chapter |
|---|---|
| GPU queues, textures, camera matrices | [Rendering and camera](rendering.md) |
| Font atlas, layout, text draw | [Text](text.md) |
| Geometry DSL and tessellation | [Mesh authoring](mesh-authoring.md) |
| Realtime synthesis/samples/tracks | [Audio](audio.md) |
| Keyboard, pointer, text and IME streams | [Input](input.md) |
| Window lifecycle, mode/title, menu and cursor chrome | [Window](window.md) |
| Widget state/focus/composition | [Widgets](widgets.md) |

Keep frame ownership explicit. Product actors may emit render/text/audio mail,
but native callbacks and presentation remain chassis responsibilities. For
visual changes, pair structural/SubstrateHarness checks with captured evidence; for
realtime audio, keep allocation and blocking work off the callback.
