#!/usr/bin/env python3
"""Turn the sweep's CSVs into the tables in RESULTS.md.

    python3 spikes/log-stream-scale/analyze.py spikes/log-stream-scale/results

Every cell is `median [min..max]` over the reps of one configuration.
"""

import csv
import statistics
import sys
from collections import defaultdict
from pathlib import Path

MODES = ["nohook", "off", "mail", "mailroot", "mail256", "buffer", "buffer4k", "shard"]


def rows(path):
    if not path.exists():
        return []
    with path.open() as handle:
        return [row for row in csv.DictReader(handle) if row.get("mode")]


def spread(values, digits=0):
    if not values:
        return "-"
    form = f"{{:.{digits}f}}"
    middle = form.format(statistics.median(values))
    return f"{middle} [{form.format(min(values))}..{form.format(max(values))}]"


def median(values):
    return statistics.median(values) if values else float("nan")


def table(header, lines):
    print("| " + " | ".join(header) + " |")
    print("|" + "|".join("---" for _ in header) + "|")
    for line in lines:
        print("| " + " | ".join(str(cell) for cell in line) + " |")
    print()


def group(data, keys):
    cells = defaultdict(list)
    for row in data:
        cells[tuple(row[key] for key in keys)].append(row)
    return cells


def number(row, key):
    return float(row[key])


def mode_order(mode):
    return MODES.index(mode) if mode in MODES else len(MODES)


def closed(directory):
    data = rows(directory / "closed.csv")
    if not data:
        return
    print("## 1. Tap closed: dispatch cost per mail\n")
    cells = group(data, ["actors", "handlers", "quiet", "mode"])
    configs = sorted({key[:3] for key in cells}, key=lambda key: (int(key[0]), int(key[1])))
    lines = []
    for config in configs:
        mails = int(config[1]) + int(config[2])
        baseline = None
        for mode in sorted({key[3] for key in cells if key[:3] == config}, key=mode_order):
            reps = cells[config + (mode,)]
            wall = [number(row, "frame_p50_us") * 1000 / mails for row in reps]
            cpu = [number(row, "cpu_s") * 1e9 / (number(row, "frames") * mails) for row in reps]
            if mode == "nohook":
                baseline = (median(wall), median(cpu))
            against = "-"
            if baseline and mode != "nohook":
                against = f"{median(wall) - baseline[0]:+.1f} / {median(cpu) - baseline[1]:+.1f}"
            kind = "logging" if int(config[1]) else "quiet"
            quartiles = statistics.quantiles(wall, n=4) if len(wall) >= 4 else [min(wall), median(wall), max(wall)]
            middle_half = f"{quartiles[0]:.1f}..{quartiles[2]:.1f}"
            lines.append(
                [config[0], f"{mails} {kind}", mode, len(reps), spread(wall, 1), middle_half, spread(cpu, 1), against]
            )
    table(
        [
            "actors",
            "handlers per frame",
            "mode",
            "reps",
            "wall ns per mail",
            "middle half of reps",
            "CPU ns per mail",
            "vs nohook (wall / CPU ns)",
        ],
        lines,
    )


def steady(directory, pace, title):
    data = [row for row in rows(directory / "steady.csv") if (row["pace_hz"] == "0") == (pace == 0)]
    if not data:
        return
    print(f"## {title}\n")
    keys = ["logpath", "actors", "handlers", "lines", "quiet", "mode"]
    cells = group(data, keys)
    configs = sorted({key[:5] for key in cells}, key=lambda key: (key[0], int(key[1]), int(key[2]), int(key[3])))
    cost, account, subscriber = [], [], []
    for config in configs:
        off = cells.get(config + ("off",), [])
        off_frame = median([number(row, "frame_p50_us") for row in off])
        off_cpu = median([number(row, "cpu_s") * 1e6 / number(row, "frames") for row in off])
        handlers = int(config[2])
        label = f"{config[1]} / {handlers}x{config[3]}" + (f" +{config[4]} quiet" if int(config[4]) else "")
        if config[0] != "direct":
            label += f" ({config[0]})"
        for mode in sorted({key[5] for key in cells if key[:5] == config}, key=mode_order):
            reps = cells[config + (mode,)]
            frame = [number(row, "frame_p50_us") for row in reps]
            worst = [number(row, "frame_p99_us") for row in reps]
            cpu = [number(row, "cpu_s") * 1e6 / number(row, "frames") for row in reps]
            added_frame = median(frame) - off_frame
            added_cpu = median(cpu) - off_cpu
            per_handler = f"{added_cpu * 1000 / handlers:.0f}" if handlers and mode != "off" else "-"
            share = f"{added_frame / 16600 * 100:+.1f}%" if mode != "off" else "-"
            cost.append(
                [label, mode, len(reps), spread(frame), spread(worst), spread(cpu), f"{added_frame:+.0f}", share, per_handler]
            )
            if mode == "off":
                continue
            total = lambda key: int(sum(number(row, key) for row in reps))
            account.append(
                [
                    label,
                    mode,
                    total("produced_total"),
                    total("delivered_total"),
                    total("skipped_total"),
                    total("dropped_total"),
                    total("cap_skipped_total"),
                    total("ring_lost_total"),
                    total("foreign_total"),
                    total("unaccounted"),
                ]
            )
            subscriber.append(
                [
                    label,
                    mode,
                    spread([number(row, "batch_p50") for row in reps]),
                    int(max(number(row, "batch_max") for row in reps)),
                    spread([number(row, "console_ns_per_batch") / 1000 for row in reps]),
                    spread([number(row, "console_ns_max") / 1000 for row in reps]),
                    spread([number(row, "gather_tick_ns_mean") / 1000 for row in reps]),
                    spread([number(row, "gather_slice_ns_ewma") for row in reps]),
                    int(max(number(row, "inbox_peak") for row in reps)),
                    int(max(number(row, "buffer_peak") for row in reps)),
                ]
            )
    print("### Cost\n")
    table(
        [
            "actors / logging handlers x lines",
            "mode",
            "reps",
            "frame p50 us",
            "frame p99 us",
            "CPU us per frame",
            "added frame us",
            "of 16.6 ms",
            "added CPU per frame / logging handlers, ns",
        ],
        cost,
    )
    print("### Accounting (summed over reps, whole run including warm-up and drain)\n")
    table(
        ["config", "mode", "produced", "delivered", "reported skipped", "of which dropped", "over batch cap", "ring wrapped", "foreign lines", "unaccounted"],
        account,
    )
    print("### Subscriber and gatherer\n")
    table(
        [
            "config",
            "mode",
            "batch lines p50",
            "batch max",
            "console us per batch",
            "console worst us",
            "gatherer tick us",
            "gatherer slice ns",
            "inbox peak",
            "buffer peak",
        ],
        subscriber,
    )


def overload(directory, name, title):
    path = directory / name
    if not path.exists():
        return
    timeline, summary = [], []
    with path.open() as handle:
        reader = csv.reader(handle)
        headers = {}
        for row in reader:
            if not row:
                continue
            if row[0] == "row":
                headers["T" if "t_s" in row else "S"] = row
            elif row[0] in headers and len(row) == len(headers[row[0]]):
                (timeline if row[0] == "T" else summary).append(dict(zip(headers[row[0]], row)))
    print(f"## {title}\n")
    by_mode = group(summary, ["mode"])
    lines = []
    for (mode,), reps in sorted(by_mode.items(), key=lambda item: mode_order(item[0][0])):
        seconds = lambda key: [float(row[key]) for row in reps if row[key] != "never"]
        never = lambda key: sum(1 for row in reps if row[key] == "never")
        lines.append(
            [
                mode,
                len(reps),
                ", ".join(sorted({row["flood_ended_by"] for row in reps})),
                spread([float(row["flood_secs"]) for row in reps], 1),
                spread([float(row["produced_per_sec"]) / 1e6 for row in reps], 2),
                spread([float(row["flood_frames"]) for row in reps]),
                spread([float(row["flood_frame_p50_us"]) / 1000 for row in reps], 2),
                spread([float(row["flood_frame_max_us"]) / 1000 for row in reps], 1),
                spread([float(row["peak_inbox"]) for row in reps]),
                spread([float(row["peak_buffer"]) for row in reps]),
                spread([float(row["peak_rss_mib"]) for row in reps]),
                spread(seconds("backlog_zero_after_s"), 2) + (f" ({never('backlog_zero_after_s')} never)" if never("backlog_zero_after_s") else ""),
                spread(seconds("frame_time_back_after_s"), 2)
                + (f" ({never('frame_time_back_after_s')} never)" if never("frame_time_back_after_s") else ""),
                "-" if mode == "off" else sum(int(row["unaccounted"]) for row in reps),
            ]
        )
    table(
        [
            "mode",
            "reps finished",
            "flood ended by",
            "flood s",
            "M lines/s produced",
            "frames in flood",
            "frame p50 ms",
            "frame max ms",
            "peak inbox",
            "peak buffer",
            "peak RSS MiB",
            "backlog zero after s",
            "frame time back after s",
            "unaccounted",
        ],
        lines,
    )
    print("Accounting, summed over the reps that finished:\n")
    lines = []
    for (mode,), reps in sorted(by_mode.items(), key=lambda item: mode_order(item[0][0])):
        if mode == "off":
            continue
        total = lambda key: sum(int(row[key]) for row in reps)
        lines.append(
            [mode]
            + [
                total(key)
                for key in [
                    "produced_total",
                    "delivered_total",
                    "skipped_total",
                    "dropped_total",
                    "cap_skipped_total",
                    "ring_lost_total",
                    "foreign_total",
                    "unaccounted",
                ]
            ]
        )
    table(["mode", "produced", "delivered", "reported skipped", "of which dropped", "over batch cap", "ring wrapped", "foreign", "unaccounted"], lines)

    print("Curve of the first rep of each mode (time from process start; the flood starts near 2 s):\n")
    seen = defaultdict(list)
    run_index = defaultdict(int)
    last_time = {}
    for row in timeline:
        mode = row["mode"]
        if mode in last_time and float(row["t_s"]) < last_time[mode]:
            run_index[mode] += 1
        last_time[mode] = float(row["t_s"])
        if run_index[mode] == 0:
            seen[mode].append(row)
    for mode in sorted(seen, key=mode_order):
        points = seen[mode]
        step = max(1, len(points) // 16)
        picked = points[::step] + ([points[-1]] if (len(points) - 1) % step else [])
        print(f"**{mode}**\n")
        table(
            ["t s", "phase", "gatherer inbox", "buffer lines", "RSS MiB", "frames done", "frame in progress ms", "M produced", "delivered", "skipped"],
            [
                [
                    row["t_s"],
                    row["phase"],
                    row["inbox_depth"],
                    row["buffer_occupancy"],
                    row["rss_mib"],
                    row["frames_done"],
                    row["frame_in_progress_ms"],
                    f"{int(row['produced']) / 1e6:.2f}",
                    row["delivered"],
                    row["skipped"],
                ]
                for row in picked
            ],
        )


def backfill(directory):
    data = rows(directory / "backfill.csv")
    if not data:
        return
    print("## 7. Opening the tap on 1000 actors holding full rings (1024 lines each)\n")
    cells = group(data, ["mode", "backfill"])
    lines = []
    for (mode, flag), reps in sorted(cells.items(), key=lambda item: (mode_order(item[0][0]), item[0][1])):
        value = lambda key: [number(row, key) for row in reps]
        quiet = [float(row["secs_until_quiet"]) for row in reps if row["secs_until_quiet"] != "never"]
        lines.append(
            [
                mode,
                "yes" if flag == "1" else "no",
                len(reps),
                spread(value("baseline_frame_p50_us")),
                spread([v / 1000 for v in value("first_frame_us")], 1),
                spread([v / 1000 for v in value("second_frame_us")], 1),
                spread([v / 1000 for v in value("worst_frame_us")], 1),
                spread(value("frames_over_16ms")),
                spread(value("peak_inbox")),
                spread([peak - before for peak, before in zip(value("rss_peak_mib"), value("rss_before_mib"))]),
                spread(value("slices")),
                spread(value("delivered")),
                spread(value("skipped")),
                spread(quiet, 2),
            ]
        )
    table(
        [
            "mode",
            "backfill",
            "reps",
            "frame before us",
            "first frame ms",
            "second frame ms",
            "worst frame ms",
            "frames over 16.6 ms",
            "peak inbox (slices)",
            "RSS rise MiB",
            "slices",
            "lines delivered",
            "lines reported skipped",
            "quiet after s",
        ],
        lines,
    )


def main():
    directory = Path(sys.argv[1] if len(sys.argv) > 1 else "results")
    closed(directory)
    steady(directory, 60, "2, 4, 5. Paced at 60 frames per second")
    steady(directory, 0, "2 (flood). Frames back to back")
    overload(directory, "overload4.csv", "3, 6. Sustained overload, 4 producers looping (half the workers idle)")
    overload(directory, "overload1000.csv", "3, 6. Sustained overload, 1000 producers looping (every worker busy)")
    backfill(directory)


if __name__ == "__main__":
    main()
