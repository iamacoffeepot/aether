#!/usr/bin/env -S uv run --quiet
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "matplotlib",
#     "numpy",
# ]
# ///
"""Plot mail-spike CSV output.

Reads broadcast.csv, bulk.csv, chain.csv, mixed.csv from this directory and
writes corresponding .png files alongside.

Run from this directory:
    uv run plot.py

Outputs are gitignored — they're per-machine, like the CSVs they're derived from.
"""

import csv
import sys
from pathlib import Path
from typing import NamedTuple

import matplotlib.pyplot as plt
import numpy as np

HERE = Path(__file__).parent
FRAME_BUDGET_US = 16_670  # 60Hz frame budget


class Row(NamedTuple):
    workload: str
    dim_a: int
    dim_b: int
    iterations: int
    total_ms: float
    fps: float
    mean_us: float
    p50_us: float
    p95_us: float
    p99_us: float


def load_csv(path: Path) -> list[Row]:
    rows: list[Row] = []
    with path.open() as f:
        reader = csv.reader(f)
        next(reader)  # header
        for r in reader:
            rows.append(
                Row(
                    r[0],
                    int(r[1]),
                    int(r[2]),
                    int(r[3]),
                    float(r[4]),
                    float(r[5]),
                    float(r[6]),
                    float(r[7]),
                    float(r[8]),
                    float(r[9]),
                )
            )
    return rows


def fmt_us(v: float) -> str:
    if v < 1.0:
        return f"{v * 1000:.0f}ns"
    if v < 1000.0:
        return f"{v:.1f}µs"
    return f"{v / 1000:.1f}ms"


def plot_matrix(
    rows: list[Row],
    dim_a_name: str,
    dim_b_name: str,
    metric: str,
    title: str,
    out: Path,
) -> None:
    dim_a_vals = sorted({r.dim_a for r in rows})
    dim_b_vals = sorted({r.dim_b for r in rows})
    grid = np.full((len(dim_a_vals), len(dim_b_vals)), np.nan)
    for r in rows:
        i = dim_a_vals.index(r.dim_a)
        j = dim_b_vals.index(r.dim_b)
        grid[i, j] = getattr(r, metric)

    fig, ax = plt.subplots(figsize=(8, 5.5))
    im = ax.imshow(grid, aspect="auto", cmap="viridis", norm="log")
    ax.set_xticks(range(len(dim_b_vals)), [str(v) for v in dim_b_vals])
    ax.set_yticks(range(len(dim_a_vals)), [str(v) for v in dim_a_vals])
    ax.set_xlabel(dim_b_name)
    ax.set_ylabel(dim_a_name)
    ax.set_title(f"{title}\n(red border = exceeds 16.67ms 60Hz budget)")

    threshold = grid.max() / 3
    for i in range(len(dim_a_vals)):
        for j in range(len(dim_b_vals)):
            v = grid[i, j]
            color = "white" if v < threshold else "black"
            ax.text(j, i, fmt_us(v), ha="center", va="center", color=color, fontsize=8)
            if v > FRAME_BUDGET_US:
                ax.add_patch(
                    plt.Rectangle(
                        (j - 0.5, i - 0.5), 1, 1, fill=False, edgecolor="red", linewidth=2
                    )
                )

    cbar = fig.colorbar(im)
    cbar.set_label(f"{metric.replace('_', ' ')}")
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    print(f"wrote {out.name}")


def plot_bulk(rows: list[Row], out: Path) -> None:
    rows = sorted(rows, key=lambda r: r.dim_a)
    batch_sizes = [r.dim_a for r in rows]
    means = [r.mean_us for r in rows]
    per_item_ns = [r.mean_us * 1000.0 / r.dim_a for r in rows]

    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(11, 4.2))

    ax1.loglog(batch_sizes, means, "o-")
    ax1.axhline(FRAME_BUDGET_US, color="red", linestyle="--", label="60Hz budget")
    ax1.set_xlabel("batch size K")
    ax1.set_ylabel("mean per-mail latency (µs)")
    ax1.set_title("bulk: per-mail cost vs batch size")
    ax1.grid(True, which="both", alpha=0.4)
    ax1.legend()

    ax2.semilogx(batch_sizes, per_item_ns, "o-")
    ax2.set_xlabel("batch size K")
    ax2.set_ylabel("amortized cost per item (ns)")
    ax2.set_title("bulk: per-item amortization")
    ax2.grid(True, which="both", alpha=0.4)

    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    print(f"wrote {out.name}")


def plot_chain(rows: list[Row], out: Path) -> None:
    rows = sorted(rows, key=lambda r: r.dim_a)
    depths = [r.dim_a for r in rows]
    means = [r.mean_us for r in rows]
    p99s = [r.p99_us for r in rows]

    fig, ax = plt.subplots(figsize=(7.5, 5))
    ax.plot(depths, means, "o-", label="mean")
    ax.plot(depths, p99s, "s--", label="p99")
    ax.set_xlabel("chain depth D")
    ax.set_ylabel("per-frame latency (µs)")
    ax.set_title("chain: latency vs depth")
    ax.grid(True, alpha=0.4)
    ax.legend()
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    print(f"wrote {out.name}")


def main() -> int:
    any_done = False
    for name, dim_a, dim_b in [
        ("broadcast", "n_actors", "work_per_actor"),
        ("mixed", "n_actors", "work_per_actor"),
    ]:
        path = HERE / f"{name}.csv"
        if not path.exists():
            print(f"missing {path.name}, skipping")
            continue
        rows = load_csv(path)
        plot_matrix(rows, dim_a, dim_b, "mean_us", f"{name}: mean per-frame latency", HERE / f"{name}_mean.png")
        plot_matrix(rows, dim_a, dim_b, "p99_us", f"{name}: p99 per-frame latency", HERE / f"{name}_p99.png")
        any_done = True

    for name, plot_fn in [("bulk", plot_bulk), ("chain", plot_chain)]:
        path = HERE / f"{name}.csv"
        if not path.exists():
            print(f"missing {path.name}, skipping")
            continue
        plot_fn(load_csv(path), HERE / f"{name}.png")
        any_done = True

    if not any_done:
        print("no CSVs found — run `cargo run --release -p aether-mail-spike-host` first")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
