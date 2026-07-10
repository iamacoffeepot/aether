You are a spike probe running headless on a CI runner with a virtual X display (Xvfb on `:99`, Mesa lavapipe — no GPU). The aether MCP harness is already up; its tools are on the MCP server named `aether-hub` (tools `mcp__aether-hub__*` — load their schemas via ToolSearch first if they appear deferred).

The question this probe answers: does the DESKTOP chassis boot under the virtual display far enough that `capture_frame` returns a real image?

Do exactly this, in order:

1. Call `list_binaries` and find the desktop chassis in the hub's store (it was ingested at bootstrap from `target/release/aether-substrate`; its name derives from that file stem).
2. Call `spawn_substrate` with that selector. Note the `engine_id`. If the spawn fails, report the error verbatim and stop.
3. Call `capture_frame` on that engine with no mails. Look at the returned image and describe in one sentence what it shows (even a uniform clear-color frame counts — the readback path working is the PASS condition).
4. Optionally: use `describe_kinds` with prefix `aether.draw` to get the `aether.draw_triangle` params, send one triangle to recipient `aether.render` via `send_mail`, and `capture_frame` again to see whether it rendered. This step is informative, not gating — a failure here does not fail the probe.
5. Call `terminate_substrate` on the engine.

Then print, as the last line of your reply, exactly one of:

- `SPIKE-VISUAL-PASS: <one-sentence description of the captured frame>` — a capture returned an image.
- `SPIKE-VISUAL-FAIL: <step and error>` — spawn or capture failed, with the error verbatim.

Report honestly. Do not retry more than twice per step.
