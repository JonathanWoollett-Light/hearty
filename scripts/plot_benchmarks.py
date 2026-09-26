#!/usr/bin/env python3
"""Draw presentation-ready charts of hearty's workshop-mods benchmark.

Run the benchmark first (`cargo bench --bench workshop_mods`), then:

    python scripts/plot_benchmarks.py [RESULTS_JSON] [--out DIR] [--theme dark|light] [--format png|svg|pdf] [--only CHART ...]

RESULTS_JSON defaults to `target/tmp/workshop-mods/results.json` and the
charts go to a `charts` directory next to it. Three headline charts:

- hero.<ext>                a poster: problems found, time to check and fix
- problems.<ext>            what hearty finds in each mod
- speed.<ext>               how quickly hearty checks and fixes each mod

and three detailed ones:

- execution_time.<ext>      every benchmark scenario, per mod
- formatting_changes.<ext>  what `--format` changes, by kind
- time_breakdown.<ext>      where `--check --lint`'s time goes

Needs matplotlib (`pip install matplotlib`).
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import sys
from dataclasses import dataclass
from pathlib import Path

try:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import FancyBboxPatch
    from matplotlib.ticker import FuncFormatter, NullLocator
except ImportError:  # pragma: no cover - depends on the environment
    sys.exit("plot_benchmarks.py needs matplotlib: pip install matplotlib")

REPO = Path(__file__).resolve().parent.parent
DEFAULT_RESULTS = REPO / "target" / "tmp" / "workshop-mods" / "results.json"

# Every chart is 16:9, 1920x1080 pixels at the default DPI.
SIZE = (12.8, 7.2)
DPI = 150

# Scenario names in the order the benchmark runs them, with their flags.
SCENARIOS = [
    ("check-lint", "--check --lint"),
    ("fix-format", "--fix --format"),
    ("lint", "--lint"),
    ("check", "--check"),
    ("fix", "--fix"),
    ("format", "--format"),
]

# How many stages `time_breakdown` shows before folding the rest into "other".
BREAKDOWN_STAGES = 7


@dataclass(frozen=True)
class Theme:
    """The colours of one look."""

    background: str
    panel: str
    border: str
    text: str
    muted: str
    grid: str
    accent: str
    missing: str
    redundant: str
    formatting: str
    descriptor: str
    check: str
    fix: str
    stages: tuple[str, ...]


THEMES = {
    "dark": Theme(
        background="#0B1020",
        panel="#141C33",
        border="#26324F",
        text="#EEF2FA",
        muted="#8C97B2",
        grid="#1E2842",
        accent="#FF4D6D",
        missing="#FF4D6D",
        redundant="#FFB020",
        formatting="#2EC4B6",
        descriptor="#9B8CFF",
        check="#4DA3FF",
        fix="#2EC4B6",
        stages=("#4DA3FF", "#2EC4B6", "#FFB020", "#FF4D6D", "#9B8CFF", "#7BD389", "#F78C6B"),
    ),
    "light": Theme(
        background="#FFFFFF",
        panel="#F4F6FB",
        border="#DDE3EE",
        text="#131A2E",
        muted="#5B6680",
        grid="#E8ECF4",
        accent="#E63956",
        missing="#E63956",
        redundant="#E09100",
        formatting="#12A594",
        descriptor="#6E5BEA",
        check="#2F7FE0",
        fix="#12A594",
        stages=("#2F7FE0", "#12A594", "#E09100", "#E63956", "#6E5BEA", "#3FA34D", "#D9643A"),
    ),
}

FONTS = ["Segoe UI", "Inter", "Helvetica Neue", "Arial", "DejaVu Sans"]
MONO = ["Cascadia Mono", "Consolas", "Menlo", "DejaVu Sans Mono"]


# --- Data -----------------------------------------------------------------------


def scenario(mod: dict, name: str) -> dict | None:
    """The mod's scenario called `name`, if it ran."""
    return next((s for s in mod["scenarios"] if s["name"] == name), None)


def median(mod: dict, name: str) -> float | None:
    """The median wall time of the mod's scenario `name`, in seconds."""
    found = scenario(mod, name)
    return found["median_secs"] if found else None


def formatting_fixes(mod: dict) -> int:
    """Everything `--format` would change in the mod."""
    return sum(change["count"] for change in mod["problems"]["format_changes"])


def problems_found(mod: dict) -> int:
    """Every problem hearty reports in the mod: missing localisations,
    redundant fields, formatting fixes and descriptor warnings."""
    problems = mod["problems"]
    return (
        problems["missing_localisations"]
        + problems["redundant_fields"]
        + formatting_fixes(mod)
        + problems["descriptor_warnings"]
    )


def lines_removed(mod: dict) -> int:
    """Lines `--format` would remove, net of the ones it adds."""
    problems = mod["problems"]
    return problems["format_deletions"] - problems["format_insertions"]


# --- Formatting -------------------------------------------------------------------


def number(value: float) -> str:
    """`12345` as `12,345`."""
    return f"{value:,.0f}"


def compact(value: float) -> str:
    """`152318` as `152k`, `4466` as `4.5k`, `1234567` as `1.2M`."""
    if abs(value) >= 1_000_000:
        return f"{value / 1_000_000:.1f}M"
    if abs(value) >= 10_000:
        return f"{value / 1_000:.0f}k"
    if abs(value) >= 1_000:
        return f"{value / 1_000:.1f}k"
    return number(value)


def round_up(value: float) -> float:
    """`value` seconds rounded up to a round figure: the next 50 ms below a
    second, the next tenth of a second above."""
    step = 0.05 if value < 1 else 0.1
    return math.floor(value / step + 1) * step


def tick_seconds(value: float, _position: object = None) -> str:
    """An axis tick in seconds, e.g. `250 ms` or `1.25 s`, never rounded."""
    if not value:
        return "0"
    if value >= 1:
        return f"{value:g} s"
    return f"{value * 1000:g} ms"


def seconds(value: float) -> str:
    """A duration in seconds, in a readable unit."""
    if value >= 10:
        return f"{value:.0f} s"
    if value >= 1:
        return f"{value:.1f} s"
    return f"{value * 1000:.0f} ms"


# --- Styling ----------------------------------------------------------------------


def apply_theme(theme: Theme) -> None:
    """Sets matplotlib's defaults to the theme."""
    plt.rcParams.update(
        {
            "font.family": "sans-serif",
            "font.sans-serif": FONTS,
            "font.monospace": MONO,
            "figure.facecolor": theme.background,
            "axes.facecolor": theme.background,
            "savefig.facecolor": theme.background,
            "axes.edgecolor": theme.border,
            "axes.labelcolor": theme.muted,
            "axes.titlecolor": theme.text,
            "text.color": theme.text,
            "xtick.color": theme.muted,
            "ytick.color": theme.text,
            "axes.grid": False,
            "axes.spines.top": False,
            "axes.spines.right": False,
            "axes.spines.left": False,
            "axes.spines.bottom": False,
            "legend.frameon": False,
            "legend.labelcolor": theme.text,
            "font.size": 11,
        }
    )


def new_figure() -> plt.Figure:
    """A 16:9 figure."""
    return plt.figure(figsize=SIZE, dpi=DPI)


def header(fig: plt.Figure, theme: Theme, title: str, subtitle: str) -> None:
    """A slide title, subtitle and brand mark along the top."""
    fig.text(0.05, 0.915, title, fontsize=26, weight="bold", color=theme.text, va="center")
    fig.text(0.05, 0.855, subtitle, fontsize=13, color=theme.muted, va="center")
    brand(fig, theme, 0.95, 0.915, "right")


def brand(fig: plt.Figure, theme: Theme, x: float, y: float, ha: str, size: float = 15) -> None:
    """The `hearty` word mark with its heart."""
    text = fig.text(x, y, "hearty", fontsize=size, weight="bold", color=theme.text, ha=ha, va="center")
    if ha == "right":
        fig.text(x, y, "hearty ", fontsize=size, weight="bold", color=theme.text, ha="right", va="center", alpha=0)
        fig.canvas.draw()
        box = text.get_window_extent().transformed(fig.transFigure.inverted())
        fig.text(box.x0 - 0.006, y, "♥", fontsize=size, color=theme.accent, ha="right", va="center", fontfamily=["Segoe UI Symbol", "DejaVu Sans"])
    else:
        fig.canvas.draw()
        box = text.get_window_extent().transformed(fig.transFigure.inverted())
        fig.text(box.x1 + 0.006, y, "♥", fontsize=size, color=theme.accent, ha="left", va="center", fontfamily=["Segoe UI Symbol", "DejaVu Sans"])


def footer(fig: plt.Figure, theme: Theme, results: dict, mods: list[dict], size: float = 9) -> None:
    """What was measured, along the bottom."""
    files = sum(mod["inputs"]["script_files"] for mod in mods)
    mib = sum(mod["inputs"]["script_bytes"] + mod["inputs"]["localisation_bytes"] for mod in mods) / 2**20
    when = datetime.datetime.fromtimestamp(results.get("generated_at_unix", 0)).strftime("%d %b %Y")
    fig.text(
        0.05,
        0.035,
        f"{number(files)} script files and {mib:,.0f} MiB of script and localisation · "
        f"median of {results.get('iterations', 0)} runs on {results.get('hardware_threads', 0)} threads · {when}",
        fontsize=size,
        color=theme.muted,
        va="center",
    )


def panel(fig: plt.Figure, theme: Theme, x: float, y: float, w: float, h: float) -> None:
    """A rounded panel behind a group of elements, in figure coordinates."""
    fig.patches.append(
        FancyBboxPatch(
            (x, y),
            w,
            h,
            boxstyle="round,pad=0,rounding_size=0.018",
            mutation_aspect=SIZE[0] / SIZE[1],
            transform=fig.transFigure,
            facecolor=theme.panel,
            edgecolor=theme.border,
            linewidth=1,
            # Behind the axes drawn on it.
            zorder=-1,
        )
    )


def style_bar_axes(ax: plt.Axes, theme: Theme) -> None:
    """Plain horizontal-bar axes: no frame, faint vertical grid."""
    ax.set_facecolor("none")
    ax.grid(axis="x", color=theme.grid, linewidth=1)
    ax.set_axisbelow(True)
    ax.tick_params(axis="both", length=0)
    ax.tick_params(axis="x", labelsize=9)


def save(fig: plt.Figure, out: Path, name: str, fmt: str) -> Path:
    """Writes the figure and closes it."""
    path = out / f"{name}.{fmt}"
    fig.savefig(path, dpi=DPI)
    plt.close(fig)
    return path


# --- Charts -----------------------------------------------------------------------


def fitted_text(fig: plt.Figure, x: float, y: float, text: str, width: float, size: float, minimum: float, **style) -> None:
    """Writes `text` at `size` points, shrunk (down to `minimum`) until it is
    at most `width` wide, in figure coordinates."""
    renderer = fig.canvas.get_renderer()
    artist = fig.text(x, y, text, fontsize=size, **style)
    while size > minimum and artist.get_window_extent(renderer).width > width * fig.bbox.width:
        size -= 0.5
        artist.set_fontsize(size)


def plot_hero(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path:
    """A poster: total problems, time to check and fix, lines removed, and a
    row per mod. It sits under the README's own title, so it has no word
    mark, and it is shown at under half its width there, so its text is
    large and there is little of it."""
    fig = new_figure()
    total = sum(problems_found(mod) for mod in mods)
    check = sum(median(mod, "check-lint") or 0 for mod in mods)
    fix = sum(median(mod, "fix-format") or 0 for mod in mods)
    removed = sum(lines_removed(mod) for mod in mods)
    files = sum(mod["inputs"]["script_files"] for mod in mods)

    fitted_text(
        fig,
        0.05,
        0.905,
        f"Found {number(total)} problems in {len(mods)} of the biggest HOI4 mods",
        0.9,
        30,
        22,
        weight="bold",
        color=theme.text,
        va="center",
    )

    tiles = [
        (number(files), "script files checked", theme.descriptor),
        (seconds(check), f"to check all {len(mods)} mods", theme.check),
        (seconds(fix), f"to fix all {len(mods)} mods", theme.fix),
        (compact(removed), "lines removed", theme.redundant),
    ]
    gap, bottom, height = 0.02, 0.56, 0.26
    width = (0.9 - 3 * gap) / 4
    inner = width - 0.04
    for index, (value, label, colour) in enumerate(tiles):
        x = 0.05 + index * (width + gap)
        panel(fig, theme, x, bottom, width, height)
        fig.patches.append(
            plt.Rectangle((x + 0.02, bottom + height - 0.035), 0.05, 0.01, transform=fig.transFigure, color=colour, zorder=1)
        )
        fitted_text(fig, x + 0.02, bottom + height * 0.5, value, inner, 46, 30, weight="bold", color=colour, va="center")
        fitted_text(fig, x + 0.02, bottom + height * 0.17, label, inner, 18, 13, color=theme.muted, va="center")

    # A row per mod: its problems as a bar split by kind, then a small table
    # of its problems and times.
    ax = fig.add_axes((0.22, 0.08, 0.34, 0.36))
    ax.set_facecolor("none")
    kinds = [
        ("formatting", theme.formatting, formatting_fixes),
        ("redundant fields", theme.redundant, lambda mod: mod["problems"]["redundant_fields"]),
        ("missing localisations", theme.missing, lambda mod: mod["problems"]["missing_localisations"]),
    ]
    rows = list(range(len(mods)))
    left = [0] * len(mods)
    for label, colour, value in kinds:
        values = [value(mod) for mod in mods]
        ax.barh(rows, values, left=left, height=0.6, color=colour, label=label)
        left = [a + b for a, b in zip(left, values)]
    ax.set_yticks(rows)
    ax.set_yticklabels([mod["name"] for mod in mods], fontsize=18, weight="bold")
    ax.invert_yaxis()
    ax.set_xticks([])
    ax.tick_params(length=0)
    ax.set_xlim(0, max(left))

    columns = [
        (0.73, "problems", theme.text, lambda mod: number(problems_found(mod))),
        (0.84, "check", theme.check, lambda mod: seconds(median(mod, "check-lint") or 0)),
        (0.95, "fix", theme.fix, lambda mod: seconds(median(mod, "fix-format") or 0)),
    ]
    header_y = fig.transFigure.inverted().transform(ax.transAxes.transform((0, 1.02)))[1] + 0.02
    # The legend shares the column headers' line, from the left margin, so
    # it stays clear of them.
    handles, labels = ax.get_legend_handles_labels()
    fig.legend(
        handles,
        labels,
        loc="center left",
        bbox_to_anchor=(0.05, header_y),
        bbox_transform=fig.transFigure,
        ncol=3,
        fontsize=15,
        handlelength=0.9,
        handleheight=0.9,
        columnspacing=1.2,
        borderaxespad=0,
        borderpad=0,
    )
    for x, title, colour, value in columns:
        fig.text(x, header_y, title, fontsize=15, color=theme.muted, ha="right", va="center")
        for row, mod in enumerate(mods):
            y = fig.transFigure.inverted().transform(ax.transData.transform((0, row)))[1]
            fig.text(x, y, value(mod), fontsize=19, weight="bold", color=colour, ha="right", va="center")

    footer(fig, theme, results, mods, size=12)
    return save(fig, out, "hero", fmt)


def plot_problems(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path:
    """Three panels, one per kind of problem, a bar per mod."""
    fig = new_figure()
    total = sum(problems_found(mod) for mod in mods)
    header(fig, theme, "What hearty finds", f"{number(total)} problems across {len(mods)} Workshop mods, found by `hearty --check --lint`")

    def pct(part: int, whole: int) -> str:
        # Rounded down, so "100%" only ever means all of them.
        return f"{math.floor(part / whole * 1000) / 10:.1f}%" if whole else "0%"

    columns = [
        (
            "Missing localisations",
            theme.missing,
            [mod["problems"]["missing_localisations"] for mod in mods],
            [f"{pct(mod['problems']['missing_localisations'], mod['problems']['localisation_keys'])} of {compact(mod['problems']['localisation_keys'])} keys" for mod in mods],
        ),
        (
            "Redundant fields",
            theme.redundant,
            [mod["problems"]["redundant_fields"] for mod in mods],
            [f"{pct(mod['problems']['redundant_fixable'], mod['problems']['redundant_fields'])} removed by --fix" for mod in mods],
        ),
        (
            "Formatting fixes",
            theme.formatting,
            [formatting_fixes(mod) for mod in mods],
            [f"in {number(mod['problems']['files_to_reformat'])} files" for mod in mods],
        ),
    ]
    width, gap = 0.28, 0.035
    for index, (title, colour, values, notes) in enumerate(columns):
        x = 0.05 + index * (width + gap)
        panel(fig, theme, x, 0.1, width, 0.68)
        fig.text(x + 0.02, 0.735, title, fontsize=14, weight="bold", color=theme.text, va="center")
        fig.text(x + 0.02, 0.685, number(sum(values)), fontsize=26, weight="bold", color=colour, va="center")
        ax = fig.add_axes((x + 0.02, 0.14, width - 0.04, 0.48))
        ax.set_facecolor("none")
        rows = list(range(len(mods)))
        peak = max(values) or 1
        ax.barh(rows, [peak] * len(mods), height=0.3, color=theme.grid)
        ax.barh(rows, values, height=0.3, color=colour)
        for row, (mod, value, note) in enumerate(zip(mods, values, notes)):
            ax.text(0, row - 0.33, mod["name"], fontsize=10.5, weight="bold", color=theme.text, va="center")
            ax.text(peak, row - 0.33, number(value), fontsize=10.5, weight="bold", color=colour, va="center", ha="right")
            ax.text(0, row + 0.3, note, fontsize=8.5, color=theme.muted, va="center")
        ax.set_xlim(0, peak)
        ax.set_ylim(len(mods) - 0.45, -0.6)
        ax.axis("off")
    footer(fig, theme, results, mods)
    return save(fig, out, "problems", fmt)


def plot_speed(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path:
    """Time to check and to fix each mod, with its throughput."""
    fig = new_figure()
    checks = [median(mod, "check-lint") or 0 for mod in mods]
    fixes = [median(mod, "fix-format") or 0 for mod in mods]
    header(
        fig,
        theme,
        f"Checks a whole mod in under {seconds(round_up(max(checks)))}",
        "Median wall time of `hearty --check --lint` and `hearty --fix --format` on each mod",
    )
    panel(fig, theme, 0.05, 0.1, 0.9, 0.68)
    ax = fig.add_axes((0.2, 0.16, 0.44, 0.52))
    style_bar_axes(ax, theme)
    height = 0.3
    rows = list(range(len(mods)))
    check_bars = ax.barh([row - 0.17 for row in rows], checks, height=height, color=theme.check, label="check and lint")
    fix_bars = ax.barh([row + 0.17 for row in rows], fixes, height=height, color=theme.fix, label="fix and format")
    for bars in (check_bars, fix_bars):
        for bar in bars:
            ax.text(
                bar.get_width() + max(fixes) * 0.015,
                bar.get_y() + bar.get_height() / 2,
                seconds(bar.get_width()),
                va="center",
                fontsize=10,
                weight="bold",
                color=theme.text,
            )
    ax.set_yticks(rows)
    ax.set_yticklabels([mod["name"] for mod in mods], fontsize=12, weight="bold")
    ax.invert_yaxis()
    ax.set_xlim(0, max(fixes) * 1.15)
    ax.xaxis.set_major_formatter(FuncFormatter(tick_seconds))
    ax.legend(loc="lower left", bbox_to_anchor=(0, 1.0), ncol=2, fontsize=10, handlelength=1, handleheight=1)

    # Throughput per mod, to the right.
    fig.text(0.7, 0.705, "files checked\nper second", fontsize=10, color=theme.muted, va="bottom")
    fig.text(0.825, 0.705, "problems found\nper second", fontsize=10, color=theme.muted, va="bottom")
    for row, (mod, check) in enumerate(zip(mods, checks)):
        y = ax.transData.transform((0, row))
        _, fy = fig.transFigure.inverted().transform(y)
        files = mod["inputs"]["script_files"] / check if check else 0
        found = problems_found(mod) / check if check else 0
        fig.text(0.7, fy, compact(files), fontsize=20, weight="bold", color=theme.check, va="center")
        fig.text(0.825, fy, compact(found), fontsize=20, weight="bold", color=theme.accent, va="center")
    footer(fig, theme, results, mods)
    return save(fig, out, "speed", fmt)


def plot_execution_time(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path:
    """Median wall time of every scenario, per mod, with min-max whiskers."""
    fig = new_figure()
    header(fig, theme, "Every action, every mod", f"Median wall time of `hearty <flags>` over {results.get('iterations', 0)} runs; whiskers from the fastest to the slowest")
    ax = fig.add_axes((0.07, 0.17, 0.88, 0.58))
    ax.set_facecolor("none")
    ax.grid(axis="y", color=theme.grid, linewidth=1)
    ax.set_axisbelow(True)
    ax.tick_params(length=0)
    palette = theme.stages
    width = 0.8 / max(len(mods), 1)
    top = 0.0
    for index, mod in enumerate(mods):
        xs, medians, low, high = [], [], [], []
        for position, (name, _) in enumerate(SCENARIOS):
            found = scenario(mod, name)
            if not found or found["median_secs"] is None:
                continue
            xs.append(position + index * width)
            medians.append(found["median_secs"])
            low.append(found["median_secs"] - found["min_secs"])
            high.append(found["max_secs"] - found["median_secs"])
            top = max(top, found["max_secs"])
        bars = ax.bar(xs, medians, width=width * 0.9, color=palette[index % len(palette)], label=mod["name"])
        ax.errorbar(xs, medians, yerr=[low, high], fmt="none", ecolor=theme.muted, elinewidth=1, capsize=2)
        for bar, value, above in zip(bars, medians, high):
            # Upright, so the labels of neighbouring bars don't run together.
            ax.text(bar.get_x() + bar.get_width() / 2, value + above + top * 0.015, seconds(value), ha="center", va="bottom", fontsize=8, color=theme.muted, rotation=90)
    ax.set_xticks([position + width * (len(mods) - 1) / 2 for position in range(len(SCENARIOS))])
    ax.set_xticklabels([flags for _, flags in SCENARIOS], fontsize=10.5, fontfamily="monospace", color=theme.text)
    ax.set_ylim(0, top * 1.2)
    ax.yaxis.set_major_formatter(FuncFormatter(tick_seconds))
    ax.legend(loc="upper left", ncol=len(mods), fontsize=10, handlelength=1, handleheight=1)
    footer(fig, theme, results, mods)
    return save(fig, out, "execution_time", fmt)


def plot_formatting_changes(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path:
    """What `--format` would change, by kind, a bar per mod, on a log scale."""
    fig = new_figure()
    total = sum(formatting_fixes(mod) for mod in mods)
    header(fig, theme, "What formatting fixes", f"{number(total)} changes `hearty --format` makes across {len(mods)} mods (log scale)")
    # Kinds of change, the most common first.
    totals: dict[str, int] = {}
    for mod in mods:
        for change in mod["problems"]["format_changes"]:
            totals[change["kind"]] = totals.get(change["kind"], 0) + change["count"]
    kinds = sorted(totals, key=lambda kind: -totals[kind])
    ax = fig.add_axes((0.24, 0.1, 0.7, 0.62))
    style_bar_axes(ax, theme)
    palette = theme.stages
    height = 0.8 / max(len(mods), 1)
    peak = 1
    for index, mod in enumerate(mods):
        counts = {change["kind"]: change["count"] for change in mod["problems"]["format_changes"]}
        positions = [row + index * height for row in range(len(kinds))]
        values = [counts.get(kind, 0) for kind in kinds]
        peak = max(peak, *values)
        bars = ax.barh(positions, values, height=height * 0.88, color=palette[index % len(palette)], label=mod["name"])
        for bar, value in zip(bars, values):
            if value:
                ax.text(value * 1.08, bar.get_y() + bar.get_height() / 2, number(value), va="center", fontsize=7.5, color=theme.muted)
    ax.set_yticks([row + height * (len(mods) - 1) / 2 for row in range(len(kinds))])
    ax.set_yticklabels(kinds, fontsize=11)
    ax.invert_yaxis()
    ax.set_xscale("log")
    ax.set_xlim(right=peak * 4)
    ax.xaxis.set_minor_locator(NullLocator())
    ax.xaxis.set_major_formatter(FuncFormatter(lambda value, _: compact(value)))
    ax.legend(loc="lower left", bbox_to_anchor=(0, 1.0), ncol=len(mods), fontsize=10, handlelength=1, handleheight=1)
    footer(fig, theme, results, mods)
    return save(fig, out, "formatting_changes", fmt)


def stage_self_times(timings: dict) -> dict[str, float]:
    """Active time spent in each stage itself (not in stages under it),
    summed by stage name across the tree."""
    stages: dict[str, float] = {}
    for row in timings["rows"]:
        name = row["path"][-1]
        own = row["self_secs"] if row["self_secs"] is not None else row["active_secs"]
        if name.startswith("…"):
            name = "other"
        stages[name] = stages.get(name, 0.0) + own
    return stages


def plot_time_breakdown(results: dict, mods: list[dict], theme: Theme, out: Path, fmt: str) -> Path | None:
    """Where `--check --lint`'s active time goes, as a share per mod."""
    profiled = [(mod, found["timings"]) for mod in mods if (found := scenario(mod, "check-lint")) and found.get("timings")]
    if not profiled:
        return None
    fig = new_figure()
    parallel = sum(timings["parallelism"] for _, timings in profiled) / len(profiled)
    header(fig, theme, "Where the time goes", f"`hearty --check --lint`, by stage; its work runs {parallel:.0f}x in parallel on average")
    totals: dict[str, float] = {}
    shares = []
    for mod, timings in profiled:
        stages = stage_self_times(timings)
        whole = sum(stages.values()) or 1
        shares.append((mod, timings, {name: secs / whole for name, secs in stages.items()}))
        for name, secs in stages.items():
            totals[name] = totals.get(name, 0.0) + secs / whole
    shown = [name for name, _ in sorted(totals.items(), key=lambda item: -item[1]) if name != "other"][:BREAKDOWN_STAGES]

    ax = fig.add_axes((0.2, 0.2, 0.5, 0.56))
    ax.set_facecolor("none")
    ax.tick_params(length=0)
    ax.set_xticks([])
    for row, (mod, timings, share) in enumerate(shares):
        left = 0.0
        other = sum(value for name, value in share.items() if name not in shown)
        for index, name in enumerate([*shown, "other"]):
            value = other if name == "other" else share.get(name, 0.0)
            colour = theme.border if name == "other" else theme.stages[index % len(theme.stages)]
            ax.barh(row, value, left=left, height=0.6, color=colour, label=name if row == 0 else None)
            if value >= 0.06:
                ax.text(left + value / 2, row, f"{value:.0%}", ha="center", va="center", fontsize=9, weight="bold", color=theme.background)
            left += value
    ax.set_yticks(range(len(shares)))
    ax.set_yticklabels([mod["name"] for mod, _, _ in shares], fontsize=12, weight="bold")
    ax.invert_yaxis()
    ax.set_xlim(0, 1)
    # Beside each bar, now the axis is laid out: its wall time and work.
    for row, (_, timings, _) in enumerate(shares):
        fig_y = fig.transFigure.inverted().transform(ax.transData.transform((0, row)))[1]
        fig.text(0.72, fig_y + 0.017, f"{seconds(timings['wall_secs'])} wall", fontsize=13, weight="bold", color=theme.text, va="center")
        fig.text(0.72, fig_y - 0.02, f"{seconds(timings['active_secs'])} of work on {timings['threads']} threads", fontsize=9.5, color=theme.muted, va="center")
    ax.legend(loc="upper center", bbox_to_anchor=(0.5, -0.04), ncol=4, fontsize=10, handlelength=1, handleheight=1)
    footer(fig, theme, results, mods)
    return save(fig, out, "time_breakdown", fmt)


def print_summary(results: dict) -> None:
    """A text table of the results."""
    print(f"{'mod':<18} {'scripts':>8} {'check+lint':>11} {'fix+format':>11} {'problems':>10} {'to reformat':>12} {'missing loc':>14} {'redundant':>10}")
    for mod in results["mods"]:
        problems = mod["problems"]
        print(
            f"{mod['name']:<18} {mod['inputs']['script_files']:>8,} {seconds(median(mod, 'check-lint') or 0):>11} "
            f"{seconds(median(mod, 'fix-format') or 0):>11} {problems_found(mod):>10,} {problems['files_to_reformat']:>12,} "
            f"{problems['missing_localisations']:>6,}/{problems['localisation_keys']:<7,} {problems['redundant_fields']:>10,}"
        )
    for skipped in results.get("skipped", []):
        print(f"skipped {skipped['name']}: {skipped['reason']}")


# Every chart, by the name `--only` takes (its file name).
CHARTS = {
    "hero": plot_hero,
    "problems": plot_problems,
    "speed": plot_speed,
    "execution_time": plot_execution_time,
    "formatting_changes": plot_formatting_changes,
    "time_breakdown": plot_time_breakdown,
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("results", nargs="?", type=Path, default=DEFAULT_RESULTS, help="the benchmark's results.json")
    parser.add_argument("--out", type=Path, help="where to write the charts (default: charts/ next to results.json)")
    parser.add_argument("--theme", default="dark", choices=sorted(THEMES), help="colour scheme (default: dark)")
    parser.add_argument("--format", default="png", choices=["png", "svg", "pdf"], help="image format (default: png)")
    parser.add_argument("--only", nargs="+", choices=sorted(CHARTS), metavar="CHART", help=f"draw only these charts ({', '.join(CHARTS)})")
    args = parser.parse_args()

    if not args.results.is_file():
        print(f"{args.results} not found; run `cargo bench --bench workshop_mods` first", file=sys.stderr)
        return 1
    results = json.loads(args.results.read_text(encoding="utf-8"))
    mods = results["mods"]
    if not mods:
        print("the results hold no benchmarked mods", file=sys.stderr)
        for skipped in results.get("skipped", []):
            print(f"skipped {skipped['name']}: {skipped['reason']}", file=sys.stderr)
        return 1
    out = args.out or args.results.parent / "charts"
    out.mkdir(parents=True, exist_ok=True)
    theme = THEMES[args.theme]
    apply_theme(theme)

    charts = [chart for name, chart in CHARTS.items() if not args.only or name in args.only]
    written = [chart(results, mods, theme, out, args.format) for chart in charts]
    print_summary(results)
    print()
    for path in written:
        if path:
            print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
