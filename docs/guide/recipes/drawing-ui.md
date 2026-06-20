# Drawing your first UI

**Class:** drive. No recompile — a running engine, a TTF asset, and a few
pieces of mail. Reach for the MCP harness (`send_mail`) or a component's
`ctx`; the steps are identical.

The `aether.ui` capability draws immediate-mode widgets in screen space. You
mail it a panel, a bar, a label, or a button each frame, and it composes them
onto the renderer's solid-quad and text surfaces the same tick. Every widget
takes a `rect` of `[x, y, width, height]` in window pixels, with `(0, 0)` at
the top-left corner. Send a widget every frame it should be on screen; stop
sending it and it vanishes — the same contract as `aether.draw_triangle`.

## 1. A backing panel

A panel is a flat-colored rectangle — the plate a HUD sits on, or a dialog
background. Mail `aether.ui.panel` to the `aether.ui` mailbox:

```jsonc
// send_mail → aether.ui  (kind: aether.ui.panel), fire-and-forget
{
  "rect": [24.0, 16.0, 240.0, 48.0],   // x, y, width, height in pixels
  "color": [0.10, 0.10, 0.13, 1.0]     // RGBA, linear; alpha scales the blend
}
```

The capability forwards it as one screen-space solid quad to `aether.render`
the same tick. Resend it every frame the panel should hold.

## 2. A progress bar

A bar draws two layers in one widget: a `track` filling the whole rect, and a
`fill` covering `frac` of the width from the left. It is the health bar, the
load meter, the cooldown sweep. Mail `aether.ui.bar`:

```jsonc
// send_mail → aether.ui  (kind: aether.ui.bar), fire-and-forget
{
  "rect": [28.0, 22.0, 232.0, 16.0],
  "frac": 0.7,                         // clamped to [0, 1] by the cap
  "track_color": [0.10, 0.10, 0.13, 1.0],
  "fill_color": [0.25, 0.82, 0.32, 1.0]
}
```

`frac` is the only value you animate frame to frame — drop it toward `0.0` and
the fill shrinks and you recolor it to taste. The locomotion kit's health HUD
is exactly a panel plate under a bar whose `fill_color` reddens as `frac`
falls.

## 3. A text label

A label needs a font. Load one through the text capability first — the same
`aether.text.load_font` from [Drawing your first text](drawing-text.md) — and
hold onto the `font_id` it replies with:

```jsonc
// send_mail → aether.text  (kind: aether.text.load_font)
{ "namespace": "assets", "path": "fonts/RobotoMono.ttf" }
// reply: { "Ok": { "font_id": 0, "name": "RobotoMono", "resident_bytes": 183700 } }
```

Then mail `aether.ui.label` with that `font_id`. The string flows from `(x, y)`
along the baseline:

```jsonc
// send_mail → aether.ui  (kind: aether.ui.label), fire-and-forget
{
  "x": 28.0,
  "y": 14.0,
  "font_id": 0,
  "text": "HEALTH",
  "size_pixels": 14.0,
  "color": [1.0, 1.0, 1.0, 1.0]
}
```

The capability forwards it as a screen-space `aether.text.draw`, so an unknown
`font_id` warn-drops in the text cap. Resend it every frame.

## 4. A button you can click

A button draws a filled rect with a centered text label and records its rect
for hit-testing. Mail `aether.ui.button` every frame, carrying a caller-stable
`id`:

```jsonc
// send_mail → aether.ui  (kind: aether.ui.button), fire-and-forget
{
  "id": 1,
  "rect": [24.0, 80.0, 120.0, 40.0],
  "color": [0.18, 0.20, 0.26, 1.0],
  "font_id": 0,
  "text": "Restart",
  "size_pixels": 18.0,
  "text_color": [1.0, 1.0, 1.0, 1.0]
}
```

A left-click inside the rect replies `aether.ui.clicked { id }` to the sender
within one frame, carrying the same `id` you drew with — your stable handle for
the widget. When buttons overlap, the topmost (last-drawn) one wins.

In a component, you receive that reply with a handler on the clicked kind:

```rust
#[handler]
fn on_clicked(&mut self, _ctx: &mut WasmCtx<'_>, click: UiClicked) {
    if click.id == RESTART_BUTTON {
        self.restart();
    }
}
```

The cap delivers `aether.ui.clicked` by `id` to the component that drew the
button, read from the host-stamped source of the button mail. Subscribe to
nothing extra — the reply routes to you because you sent the button.

## 5. See it

From the MCP harness, `capture_frame` with the widgets in `mails` renders a
frame with the HUD composited on top:

```jsonc
// capture_frame
{
  "mails": [
    { "recipient_name": "aether.ui", "kind_name": "aether.ui.panel",
      "params": { "rect": [24.0, 16.0, 240.0, 48.0], "color": [0.10, 0.10, 0.13, 1.0] } },
    { "recipient_name": "aether.ui", "kind_name": "aether.ui.bar",
      "params": { "rect": [28.0, 22.0, 232.0, 16.0], "frac": 0.7,
                  "track_color": [0.10, 0.10, 0.13, 1.0],
                  "fill_color": [0.25, 0.82, 0.32, 1.0] } }
  ]
}
```

## What it does not do yet

- **Screen space only.** Widgets anchor in window pixels; there is no
  world-anchored panel or bar in the vocabulary yet. (Text alone reaches world
  space through `aether.text.draw`'s `World` mode.)
- **Buttons activate on left-press.** The mouse-button stream carries no button
  discriminant or release in v1, so a button fires on left-press, and only a
  left one.
- **Layout is yours.** Each widget takes an explicit rect — there is no
  flow, stacking, or anchoring layer above these kinds.

All of these sit behind the `aether.ui.*` kinds, so the internals can grow
without changing the mail you send.
