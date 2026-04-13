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


def plot_speedup(workload_to_rows: dict[str, list[Row]], out: Path) -> None:
    """Speedup vs K per N, one subplot per workload. Speedup = T(K=1) / T(K).
    Ideal linear speedup is drawn as a dashed reference line."""
    names = list(workload_to_rows.keys())
    fig, axes = plt.subplots(1, len(names), figsize=(5.2 * len(names), 4.4), sharey=True)
    if len(names) == 1:
        axes = [axes]

    for ax, name in zip(axes, names):
        rows = workload_to_rows[name]
        ns = sorted({r.dim_a for r in rows})
        ks = sorted({r.dim_b for r in rows})
        for n in ns:
            t_by_k = {r.dim_b: r.mean_us for r in rows if r.dim_a == n}
            if 1 not in t_by_k:
                continue
            baseline = t_by_k[1]
            xs = [k for k in ks if k in t_by_k]
            ys = [baseline / t_by_k[k] for k in xs]
            ax.plot(xs, ys, "o-", label=f"N={n}")
        max_k = max(ks)
        ax.plot([1, max_k], [1, max_k], "k--", alpha=0.3, label="ideal")
        ax.set_xscale("log", base=2)
        ax.set_xlabel("workers K")
        ax.set_title(name)
        ax.grid(True, which="both", alpha=0.4)
        ax.set_xticks(ks)
        ax.set_xticklabels([str(k) for k in ks])
    axes[0].set_ylabel("speedup vs K=1")
    axes[-1].legend(loc="upper left", fontsize=8)
    fig.suptitle("scheduler: speedup vs worker count")
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    print(f"wrote {out.name}")


def plot_dispatch_floor(rows: list[Row], out: Path) -> None:
    """Per-tick cost = mean_us / N. For churn (work=10), this isolates the
    scheduler's end-to-end cost per dispatched tick. Flat curves at low K
    that rise with K are the signature of shared-queue contention."""
    ns = sorted({r.dim_a for r in rows})
    ks = sorted({r.dim_b for r in rows})

    fig, ax = plt.subplots(figsize=(7.5, 5))
    for n in ns:
        xs, ys = [], []
        for k in ks:
            match = [r for r in rows if r.dim_a == n and r.dim_b == k]
            if not match:
                continue
            xs.append(k)
            ys.append(match[0].mean_us * 1000.0 / n)  # µs → ns, per-tick
        ax.plot(xs, ys, "o-", label=f"N={n}")

    ax.set_xscale("log", base=2)
    ax.set_xlabel("workers K")
    ax.set_ylabel("per-tick cost (ns)")
    ax.set_title("churn: per-tick scheduler cost vs K\n(rising curves = shared-queue contention)")
    ax.set_xticks(ks)
    ax.set_xticklabels([str(k) for k in ks])
    ax.grid(True, which="both", alpha=0.4)
    ax.legend()
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    print(f"wrote {out.name}")


def main() -> int:
    any_done = False

    # Sequential-spike CSVs (issue #7 / ADR-0003).
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

    # Concurrent-spike CSVs (issue #14 / ADR-0004).
    concurrent_workloads = ["parallel_broadcast", "parallel_mixed", "churn"]
    concurrent_rows: dict[str, list[Row]] = {}
    for name in concurrent_workloads:
        path = HERE / f"{name}.csv"
        if not path.exists():
            print(f"missing {path.name}, skipping")
            continue
        rows = load_csv(path)
        concurrent_rows[name] = rows
        plot_matrix(rows, "n_actors", "k_workers", "mean_us", f"{name}: mean per-frame latency", HERE / f"{name}_mean.png")
        plot_matrix(rows, "n_actors", "k_workers", "p99_us", f"{name}: p99 per-frame latency", HERE / f"{name}_p99.png")
        any_done = True

    if concurrent_rows:
        plot_speedup(concurrent_rows, HERE / "scheduler_speedup.png")
    if "churn" in concurrent_rows:
        plot_dispatch_floor(concurrent_rows["churn"], HERE / "churn_dispatch_floor.png")

    if not any_done:
        print(
            "no CSVs found — run `cargo run --release -p aether-mail-spike-host` and/or\n"
            "`cargo run --release -p aether-mail-spike-host --bin concurrent` first"
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
