#!/usr/bin/env python3
"""Render a headless Claude stream-json session into a readable CI log.

Reads the stream on stdin, writes a human-legible transcript on stdout, and is a
pure pass-through in the sense that it never swallows a line it cannot parse. The
point is the tool ARGUMENTS: a log that says only "ran Bash" fifty times tells a
reader nothing about what the agent did, which is the whole reason to keep a log.
"""

import json
import os
import sys

# GitHub Actions renders ANSI. Honour NO_COLOR, and drop colour when piped to a
# file so an artifact isn't littered with escape codes.
COLOR = os.environ.get("NO_COLOR") is None and (sys.stdout.isatty() or os.environ.get("GITHUB_ACTIONS") == "true")


def paint(code, s):
    return f"\033[{code}m{s}\033[0m" if COLOR else s


BOLD = "1"
OK, ERROR, INFO, WARN, MUTED = "38;5;151", "38;5;210", "38;5;117", "38;5;222", "38;5;245"

TOOL_COL_WIDTH = 12  # fixed so the arg column lines up across tool names

# How to summarise each tool's input: the parameters a reader actually needs to
# know what happened. Anything not listed falls back to a compact key=value line.
SUMMARY = {
    "Bash": lambda i: i.get("command", ""),
    "Read": lambda i: i.get("file_path", "")
    + (f":{i['offset']}-{i['offset'] + i.get('limit', 0)}" if i.get("offset") else ""),
    "Write": lambda i: i.get("file_path", ""),
    "Edit": lambda i: i.get("file_path", ""),
    "Glob": lambda i: i.get("pattern", ""),
    "Grep": lambda i: i.get("pattern", ""),
    "Skill": lambda i: " ".join(x for x in (i.get("skill"), i.get("args")) if x),
    "Agent": lambda i: f"[{i.get('subagent_type', '?')}] {i.get('description', '')}",
    "ToolSearch": lambda i: i.get("query", ""),
    "TaskCreate": lambda i: i.get("subject", ""),
    "TaskUpdate": lambda i: f"{i.get('taskId', '')} -> {i.get('status', '')}",
    "WebFetch": lambda i: i.get("url", ""),
}

MAX = 220  # a tool line is a summary, not a payload; the artifact has the full text


def flat(s, limit=MAX):
    s = " ".join(str(s).split())
    return s if len(s) <= limit else s[: limit - 1] + "…"


def describe(name, inp):
    if not isinstance(inp, dict):
        return flat(inp)
    if name in SUMMARY:
        return flat(SUMMARY[name](inp))
    scalars = {k: v for k, v in inp.items() if isinstance(v, (str, int, float, bool))}
    return flat(" ".join(f"{k}={v}" for k, v in scalars.items()))


def main():
    tools = {}  # tool_use_id -> name, so a result can be labelled with its call
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            print(line, flush=True)
            continue

        kind = ev.get("type")

        if kind == "assistant":
            for block in ev.get("message", {}).get("content", []) or []:
                if block.get("type") == "text" and block.get("text", "").strip():
                    print("\n" + paint(BOLD, "● ") + block["text"].strip() + "\n", flush=True)
                elif block.get("type") == "tool_use":
                    name = block.get("name", "?")
                    tools[block.get("id")] = name
                    col = paint(INFO, "▸ " + name.ljust(TOOL_COL_WIDTH))
                    print(f"  {col}  {describe(name, block.get('input', {}))}", flush=True)

        elif kind == "user":
            # Tool results arrive as user messages. A call whose outcome is invisible
            # is only half a log — but the useful part of an outcome is tool-specific.
            # Echoing the head of a file that was just Read by path is pure noise;
            # the head of a command's OUTPUT is what a reader came for. So only the
            # tools whose result carries information the call site doesn't already
            # give away get a body, and everything else gets pass/fail and a size.
            for block in ev.get("message", {}).get("content", []) or []:
                if block.get("type") != "tool_result":
                    continue
                name = tools.get(block.get("tool_use_id"), "")
                content = block.get("content")
                if isinstance(content, list):
                    content = " ".join(c.get("text", "") for c in content if isinstance(c, dict))
                content = content or ""
                err = block.get("is_error")

                if err:
                    print(f"      {paint(ERROR, '✗ ↳')} {flat(content, 200)}", flush=True)
                elif name in ("Bash", "Grep", "Glob", "ToolSearch"):
                    print(f"      {paint(OK, '✓ ↳')} {paint(MUTED, flat(content, 160) or '(no output)')}", flush=True)
                else:
                    lines = content.count("\n") + 1 if content else 0
                    print(f"      {paint(OK, '✓ ↳')} {paint(MUTED, f'{lines} lines')}", flush=True)

        elif kind == "result":
            u = ev.get("usage", {}) or {}
            ok = not ev.get("is_error")
            head = paint(OK if ok else ERROR, f"result: {ev.get('subtype', '?')}")
            print("\n" + paint(BOLD, "── " + head))
            print(
                paint(
                    MUTED,
                    f"   {ev.get('num_turns', 0)} turns · "
                    f"{ev.get('duration_ms', 0) / 60000:.1f} min · "
                    f"${ev.get('total_cost_usd', 0):.2f} · "
                    f"{u.get('output_tokens', 0):,} out / {u.get('cache_read_input_tokens', 0):,} cached",
                ),
                flush=True,
            )
            if ev.get("result"):
                print("\n" + str(ev["result"]).strip() + "\n", flush=True)


if __name__ == "__main__":
    main()
