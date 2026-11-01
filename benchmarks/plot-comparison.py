#!/usr/bin/env python3
"""Render the two README charts. Install matplotlib, then run this script from any directory."""
import argparse
import json
import statistics
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Patch
from matplotlib.ticker import MaxNLocator, StrMethodFormatter

ROOT = Path(__file__).resolve().parents[1]
METHODS = (
    ("git", "Git worktree"),
    ("rift", "Rift (--copy-all)"),
    ("worktree-fast", "git-worktree-fast"),
)
INK, MUTED, GRID = "#152a38", "#4d6370", "#d4dfe0"
TEAL, SLATE = "#007b72", "#596c91"
MIB = 1024 ** 2


def frame(machine, title, heading, legend, footer, height, *, left):
    fig, ax = plt.subplots(figsize=(6.6, height))
    fig.patch.set_facecolor("white")
    fig.subplots_adjust(left=left, right=0.965, top=0.69, bottom=0.22)
    fig.text(0.045, 0.94, f"WORKTREE BENCHMARK   /   {machine.upper()}", color=MUTED,
             fontsize=9.5, weight="bold", va="center")
    fig.text(0.045, 0.84, title, color=INK, fontsize=17.8, weight="bold", va="center")
    fig.legend(handles=legend, loc="center left", bbox_to_anchor=(0.045, 0.735),
               frameon=False, ncol=len(legend), handlelength=1.2, columnspacing=1.8,
               handletextpad=0.5, borderaxespad=0, labelcolor=INK, fontsize=11.1)
    fig.text(0.045, 0.067, footer, color=MUTED, fontsize=9.6, va="center", linespacing=1.4)
    ax.set_axisbelow(True)
    ax.grid(axis="x", color=GRID, linewidth=0.7)
    ax.axvline(0, color=MUTED, linewidth=0.75, zorder=2)
    for spine in ax.spines.values():
        spine.set_visible(False)
    ax.tick_params(axis="both", length=0, colors=MUTED, labelsize=10.5, pad=5)
    ax.set_xlabel(heading, fontsize=10.5, color=MUTED, labelpad=5)
    return fig, ax


def value(ax, x, y, label):
    ax.annotate(label, (x, y), xytext=(4 if x >= 0 else -4, 0), textcoords="offset points",
                va="center", ha="left" if x >= 0 else "right", fontsize=11.1, color=INK)


def speed(data):
    samples = data["timing"]["samples"]
    by_method = [[r for r in samples if r["method"] == key] for key, _ in METHODS]
    trials = len(by_method[0])
    if not trials or not all(len(group) == trials for group in by_method):
        raise ValueError("The timing chart requires balanced, nonempty trials")
    footer = (f"Median of {trials} warm trials per method · {data['tracked_paths']:,} tracked paths on APFS\n"
              "Opening includes first full Git status. Removal includes Rift garbage collection.")
    fig, ax = frame(data["machine"], "Time to open or remove a workspace", "Elapsed seconds",
                    [Patch(color=TEAL, label="Open and check"), Patch(color=SLATE, label="Remove")],
                    footer, 4.28, left=0.28)
    longest = 0
    for i, group in enumerate(by_method):
        for offset, field, color in [(-0.18, "ready_s", TEAL), (0.18, "remove_s", SLATE)]:
            seconds = statistics.median(r[field] for r in group)
            y = i + offset
            ax.barh(y, seconds, height=0.29, color=color)
            value(ax, seconds, y, f"{seconds:.2f}")
            longest = max(longest, seconds)
    ax.set_yticks(range(len(METHODS)), [label for _, label in METHODS], color=INK, fontsize=11)
    ax.set_ylim(len(METHODS) - 0.5, -0.5)
    ax.set_xlim(0, longest * 1.18)
    ax.xaxis.set_major_locator(MaxNLocator(nbins=6, steps=[1, 2, 2.5, 5, 10]))
    return fig


def storage(data):
    copies = data["storage"]["workspace_count"]
    positions = [(0, 1)] if copies == 1 else [(-0.19, 1), (0.19, copies)]
    caption = "Each row shows one live workspace." if copies == 1 else f"Each pair shows 1 and {copies} live workspaces."
    footer = ("Dedicated APFS volume · Additional MiB above the existing source checkout\n"
              f"End labels include preparation. {caption}")
    fig, ax = frame(data["machine"], "Disk space with live workspaces", "Additional MiB (1 MiB = 1,048,576 bytes)",
                    [Patch(color=SLATE, label="Retained preparation"), Patch(color=TEAL, label="Live copies")],
                    footer, 4.60, left=0.32)
    longest = 0
    for i, (key, label) in enumerate(METHODS):
        measure = data["storage"]["methods"][key]
        prepared = (measure["prepared_used_bytes"] - measure["baseline_used_bytes"]) / MIB
        ax.text(-0.064, i, label, va="center", ha="right", transform=ax.get_yaxis_transform(),
                color=INK, fontsize=11)
        for offset, count in positions:
            live = (measure["live_used_bytes"][count - 1] - measure["prepared_used_bytes"]) / MIB
            if min(live, prepared) < 0:
                raise ValueError("Negative storage deltas cannot be plotted as additive bars")
            y = i + offset
            ax.text(-0.012, y, f"{count}×", va="center", ha="right",
                    transform=ax.get_yaxis_transform(), color=MUTED, fontsize=10)
            ax.barh(y, prepared, height=0.30, color=SLATE)
            ax.barh(y, live, left=prepared, height=0.30, color=TEAL)
            total = prepared + live
            value(ax, total, y, f"{total:,.1f}")
            longest = max(longest, total)
    ax.set_yticks([])
    ax.set_ylim(len(METHODS) - 0.5, -0.5)
    ax.set_xlim(0, longest * 1.22)
    ax.xaxis.set_major_locator(MaxNLocator(nbins=5, integer=True))
    ax.xaxis.set_major_formatter(StrMethodFormatter("{x:,.0f}"))
    return fig


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=ROOT / "benchmarks/t3-git-rift-fast.json")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "docs/assets")
    args = parser.parse_args()
    data = json.loads(args.input.read_text(encoding="utf-8"))
    plt.style.use("default")
    plt.rcParams["font.family"] = "DejaVu Sans"
    args.output_dir.mkdir(parents=True, exist_ok=True)
    charts = [(name, draw) for key, name, draw in
              [("timing", "speed", speed), ("storage", "storage", storage)] if key in data]
    if not charts:
        parser.error("report has no timing or storage measurements")
    for name, draw in charts:
        fig = draw(data)
        fig.savefig(args.output_dir / f"workspace-{name}.png", dpi=200, facecolor="white",
                    metadata={"Software": "worktree-fast benchmark plots"})
        plt.close(fig)


if __name__ == "__main__":
    main()
