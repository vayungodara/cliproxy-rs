#!/usr/bin/env python3
"""Draws the README memory chart from a field-mix soak results file (bench/soak.sh).

    python3 bench/chart.py [results.jsonl] [output dir]

Defaults: bench/results/2026-10-28-field-go.jsonl and docs/img. Writes
perf-field-soak-light.svg and perf-field-soak-dark.svg, and prints the alt text for
the README. Only measured values are drawn: peak RSS (hwm_kb) and RSS after the 300
seconds idle that follow the load (idle_rss_kb), in MB of 1,024 kB as /proc reports
them. The idle reading is the resting value: one reading per server in the same state,
five minutes without traffic, like the earlier chart's RSS after the load. The readings
20 seconds after each batch (rest_rss_kb) are a range over the hour, and Go returns
freed heap to the kernel over minutes, so they mix live and freed memory differently on
the two servers; the README text gives them next to the chart.

To redraw from a new soak, point SESSION and RUNS at its labels and update SUBTITLE.
"""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench/results/2026-10-28-field-go.jsonl"
SESSION = "ci-runners-60min"
TITLE = "Memory in the field-mix soak: CLIProxyAPI v8.0.35 (Go) and cliproxy-rs 0.4.0"
SUBTITLE = ["28 Oct 2026: both release binaries, 60 minutes of Claude, Codex and count_tokens requests,",
            "on two GitHub-hosted Ubuntu 22.04 runners (4 vCPUs each) at the same time."]
# (panel title, [(server, run label, round label)])
RUNS = [
    ("Requests of 200 KB to 2 MB, about 1 MB on average",
     [("go", "go-8.0.35", ""), ("rs", "0.4.0", "")]),
]
METRICS = [("hwm_kb", "Peak RSS", "peak RSS"), ("idle_rss_kb", "RSS after 300 s idle", "RSS after 300 s idle")]
NAMES = {"go": "CLIProxyAPI v8.0.35 (Go)", "rs": "cliproxy-rs 0.4.0"}
THEMES = {
    "light": {"text": "#1f2328", "muted": "#59636e", "grid": "#d1d9e0", "go": "#818b98", "rs": "#0969da"},
    "dark": {"text": "#f0f6fc", "muted": "#9198a1", "grid": "#3d444d", "go": "#656c76", "rs": "#4493f8"},
}
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"


def load(path):
    rows = {}
    for line in Path(path).read_text().splitlines():
        if line.strip():
            row = json.loads(line)
            if row.get("session") == SESSION:
                rows[row["label"]] = row
    panels = []
    for title, runs in RUNS:
        bars = []
        for metric, metric_name, _ in METRICS:
            for server, label, round_label in runs:
                if label not in rows:
                    sys.exit(f"{path}: no run labelled {label!r} in session {SESSION!r}")
                bars.append((metric_name, server, round_label, rows[label][metric] / 1024))
        panels.append((title, bars))
    return panels


def mb(value):
    return f"{value:.1f}"


def alt_text(panels):
    parts = []
    for title, bars in panels:
        groups = []
        for _, metric_name, alt_name in METRICS:
            values = {}
            for name, server, _, value in bars:
                if name == metric_name:
                    values.setdefault(server, []).append(mb(value))
            groups.append(f"{alt_name}: Go {' and '.join(values['go'])} MB, "
                          f"cliproxy-rs {' and '.join(values['rs'])} MB")
        parts.append(f"{title}, {'; '.join(groups)}")
    return ("Bar chart of the field-mix soak on 28 October 2026, 60 minutes on two runners at the same time. "
            + ". ".join(parts) + ".")


def svg(panels, theme):
    c = THEMES[theme]
    width, height = 880, 420
    top, bottom = 140, 360
    top_mb = max(value for _, bars in panels for *_, value in bars)
    # Room above the tallest bar for its label.
    scale_max = -(-(top_mb + 15) // 50) * 50
    y = lambda value: bottom - (bottom - top) * value / scale_max
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
           f'viewBox="0 0 {width} {height}" font-family="{FONT}">',
           f'<text x="24" y="34" font-size="19" font-weight="600" fill="{c["text"]}">{TITLE}</text>',
           *(f'<text x="24" y="{58 + 18 * i}" font-size="13" fill="{c["muted"]}">{line}</text>'
             for i, line in enumerate(SUBTITLE))]
    legend_x = 24
    for server in ("go", "rs"):
        out.append(f'<rect x="{legend_x}" y="94" width="12" height="12" rx="2" fill="{c[server]}"/>')
        out.append(f'<text x="{legend_x + 18}" y="105" font-size="13" fill="{c["text"]}">{NAMES[server]}</text>')
        legend_x += 210
    out.append(f'<text x="{legend_x}" y="105" font-size="13" fill="{c["muted"]}">MB, lower is better</text>')
    left = 64
    for tick in range(0, int(scale_max) + 1, 50):
        out.append(f'<line x1="{left}" x2="{width - 24}" y1="{y(tick):.1f}" y2="{y(tick):.1f}" '
                   f'stroke="{c["grid"]}" stroke-width="1"/>')
        out.append(f'<text x="{left - 8}" y="{y(tick) + 4:.1f}" font-size="11" text-anchor="end" '
                   f'fill="{c["muted"]}">{tick}</text>')
    panel_x = left + 16
    panel_width = (width - 24 - panel_x) // len(panels)
    for title, bars in panels:
        out.append(f'<text x="{panel_x}" y="130" font-size="13" font-weight="600" fill="{c["text"]}">{title}</text>')
        per_metric = len(bars) // len(METRICS)
        # The metric groups spread over the panel.
        bar_w, gap = 100, 10
        group_gap = (panel_width - len(METRICS) * (per_metric * (bar_w + gap) - gap)) // len(METRICS)
        x = panel_x + group_gap // 2
        for m, (_, metric_name, _) in enumerate(METRICS):
            group = bars[m * per_metric:(m + 1) * per_metric]
            group_w = per_metric * bar_w + (per_metric - 1) * gap
            for _, server, round_label, value in group:
                out.append(f'<rect x="{x}" y="{y(value):.1f}" width="{bar_w}" height="{bottom - y(value):.1f}" '
                           f'rx="2" fill="{c[server]}"><title>{NAMES[server]} {round_label} {mb(value)} MB</title></rect>')
                out.append(f'<text x="{x + bar_w / 2}" y="{y(value) - 5:.1f}" font-size="11" text-anchor="middle" '
                           f'fill="{c["text"]}">{mb(value)}</text>')
                x += bar_w + gap
            out.append(f'<text x="{x - gap - group_w / 2:.1f}" y="{bottom + 20}" font-size="12" '
                       f'text-anchor="middle" fill="{c["text"]}">{metric_name}</text>')
            x += group_gap - gap
        panel_x += panel_width
    out.append(f'<text x="24" y="{height - 12}" font-size="11" fill="{c["muted"]}">'
               f'Methods: docs/BENCHMARKS.md. '
               f'Raw results: bench/results/.</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    results = Path(sys.argv[1]) if len(sys.argv) > 1 else RESULTS
    out_dir = Path(sys.argv[2]) if len(sys.argv) > 2 else ROOT / "docs/img"
    panels = load(results)
    for theme in THEMES:
        (out_dir / f"perf-field-soak-{theme}.svg").write_text(svg(panels, theme))
    print(alt_text(panels))


if __name__ == "__main__":
    main()
