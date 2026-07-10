You are a spike probe running headless on a CI runner. The aether MCP harness is already up; its tools are on the MCP server named `aether-hub` (tools `mcp__aether-hub__*` — load their schemas via ToolSearch first if they appear deferred).

Do exactly this, in order:

1. Call `list_engines` and report what it returns.
2. Call `spawn_substrate` with no selector (the headless default chassis). Note the `engine_id`.
3. Call `describe_kinds` on that engine with prefix `aether.fs` and confirm the fs kind family is present.
4. Via `send_mail`, send kind `aether.fs.write` to recipient `aether.fs` on that engine, writing the text `dogfood-runner-spike` to namespace `save`, path `spike.txt` (the `bytes` field accepts `{"$text": "dogfood-runner-spike"}`).
5. Via `send_mail`, send kind `aether.fs.read` to recipient `aether.fs` for the same namespace + path, and verify the returned bytes round-trip to `dogfood-runner-spike`.
6. Call `terminate_substrate` on the engine.

Then print, as the last line of your reply, exactly one of:

- `SPIKE-DRIVE-PASS` — every step succeeded and the bytes round-tripped.
- `SPIKE-DRIVE-FAIL: <step and error>` — the first step that failed, with the error verbatim.

Report honestly. Do not retry more than twice per step. Do not touch the filesystem of the runner itself; everything goes through the MCP tools.
