#!/usr/bin/env python3
"""Box-plot report for benchmark results (pure Python, no dependencies).

Reads the JSON written by ``bench.py --json`` and renders one box plot per
bench: the y axis is that bench's throughput metric and each box is one
target's spread over its repeated samples. Boxes need a distribution, so run
the benchmarks with several repeats (``bench.py --repeat 5``, the default).

    python bench/bench.py --json bench/result.json --plot bench/report.html
    python bench/plot.py bench/result.json -o bench/report.html

Output is a single self-contained ``.html`` with inline SVG -- no build step
and no plotting library.
"""

from __future__ import annotations

import argparse
import html
import json
import statistics
import sys

PALETTE = [
    "#4c8dff",
    "#f2711c",
    "#21ba45",
    "#a333c8",
    "#00b5ad",
    "#db2828",
    "#b5cc18",
    "#6435c9",
    "#767676",
    "#e03997",
]


def fmt_num(value) -> str:
    """Compact, human-readable number for axis ticks and cells."""
    if value is None:
        return "-"
    value = float(value)
    if value == 0:
        return "0"
    if abs(value) >= 1000:
        return "{:,.0f}".format(value)
    if abs(value) >= 100:
        return "{:,.0f}".format(value)
    if abs(value) >= 1:
        return "{:,.2f}".format(value)
    return "{:.3g}".format(value)


def box_stats(values: list) -> dict:
    """Standard (Tukey) box statistics: quartiles, 1.5*IQR whiskers, outliers."""
    ordered = sorted(float(v) for v in values)
    if len(ordered) == 1:
        only = ordered[0]
        return {"q1": only, "median": only, "q3": only, "low": only, "high": only,
                "outliers": [], "values": ordered}
    q1, median, q3 = statistics.quantiles(ordered, n=4, method="inclusive")
    iqr = q3 - q1
    lo_fence, hi_fence = q1 - 1.5 * iqr, q3 + 1.5 * iqr
    inside = [v for v in ordered if lo_fence <= v <= hi_fence]
    return {
        "q1": q1,
        "median": median,
        "q3": q3,
        "low": min(inside),
        "high": max(inside),
        "outliers": [v for v in ordered if v < lo_fence or v > hi_fence],
        "values": ordered,
    }


def pick_metric(samples: list) -> str | None:
    """The bench's headline metric: a ``*/s`` rate, else seconds, else any number."""
    keys: set[str] = set()
    for sample in samples:
        keys.update(sample)
    for key in sorted(keys):
        if key.endswith("/s"):
            return key
    if "seconds" in keys:
        return "seconds"
    for key in sorted(keys):
        if any(isinstance(s.get(key), (int, float)) for s in samples):
            return key
    return None


def direction(metric: str) -> str:
    return "lower is better" if metric == "seconds" else "higher is better"


def svg_boxplot(groups: list, metric: str, palette: dict | None = None,
                width: int = 760, height: int = 340) -> str:
    """One box per ``(label, values)`` group, sharing a linear y axis.

    ``palette`` maps a group label to its colour; it must come from the target
    order so a skipped target cannot shift the remaining colours.
    """
    palette = palette or {}
    left, right, top, bottom = 68, 20, 26, 62
    plot_w = width - left - right
    plot_h = height - top - bottom

    all_values = [v for _, values in groups for v in values]
    lo, hi = min(all_values), max(all_values)
    if hi <= lo:
        span = abs(lo) * 0.1 or 1.0
        lo, hi = lo - span, hi + span
    pad = (hi - lo) * 0.08
    lo, hi = lo - pad, hi + pad

    def y(value: float) -> float:
        return top + plot_h * (1 - (value - lo) / (hi - lo))

    out = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 %d %d" role="img" '
        'font-family="ui-sans-serif, system-ui, -apple-system, sans-serif" font-size="12">'
        % (width, height),
        '<rect width="%d" height="%d" fill="#ffffff"/>' % (width, height),
    ]

    ticks = 5
    for i in range(ticks + 1):
        value = lo + (hi - lo) * i / ticks
        yy = y(value)
        out.append('<line x1="%d" y1="%.1f" x2="%d" y2="%.1f" stroke="#eceef1"/>' % (left, yy, left + plot_w, yy))
        out.append('<text x="%d" y="%.1f" text-anchor="end" dominant-baseline="middle" fill="#5b6470">%s</text>'
                   % (left - 8, yy, fmt_num(value)))
    out.append('<line x1="%d" y1="%d" x2="%d" y2="%d" stroke="#c3c8cf"/>' % (left, top, left, top + plot_h))
    out.append('<line x1="%d" y1="%d" x2="%d" y2="%d" stroke="#c3c8cf"/>'
               % (left, top + plot_h, left + plot_w, top + plot_h))
    out.append('<text transform="translate(16,%.1f) rotate(-90)" text-anchor="middle" fill="#5b6470">%s</text>'
               % (top + plot_h / 2, html.escape(metric)))

    slot = plot_w / len(groups)
    box_w = max(16.0, min(58.0, slot * 0.5))
    cap = box_w * 0.35
    for i, (label, values) in enumerate(groups):
        cx = left + slot * (i + 0.5)
        color = palette.get(label) or PALETTE[i % len(PALETTE)]
        stats = box_stats(values)
        out.append('<line x1="%.1f" y1="%.1f" x2="%.1f" y2="%.1f" stroke="%s" stroke-width="1.5"/>'
                   % (cx, y(stats["high"]), cx, y(stats["low"]), color))
        for whisk in (stats["high"], stats["low"]):
            out.append('<line x1="%.1f" y1="%.1f" x2="%.1f" y2="%.1f" stroke="%s" stroke-width="1.5"/>'
                       % (cx - cap, y(whisk), cx + cap, y(whisk), color))
        q1y, q3y = y(stats["q1"]), y(stats["q3"])
        out.append('<rect x="%.1f" y="%.1f" width="%.1f" height="%.1f" fill="%s" fill-opacity="0.16" '
                   'stroke="%s" stroke-width="1.5"/>'
                   % (cx - box_w / 2, min(q1y, q3y), box_w, abs(q1y - q3y), color, color))
        out.append('<line x1="%.1f" y1="%.1f" x2="%.1f" y2="%.1f" stroke="%s" stroke-width="2.5"/>'
                   % (cx - box_w / 2, y(stats["median"]), cx + box_w / 2, y(stats["median"]), color))
        count = len(values)
        for k, value in enumerate(stats["values"]):
            offset = 0.0 if count == 1 else (k / (count - 1) - 0.5) * box_w * 0.7
            out.append('<circle cx="%.1f" cy="%.1f" r="2.6" fill="%s" fill-opacity="0.85"/>'
                       % (cx + offset, y(value), color))
        out.append('<text x="%.1f" y="%d" text-anchor="middle" fill="#2b3138">%s</text>'
                   % (cx, top + plot_h + 20, html.escape(str(label))))

    out.append("</svg>")
    return "".join(out)


def _collect(payload: dict) -> list:
    """Group ok records into ``(bench, metric, [(target, values), ...])``."""
    target_order = {t["id"]: i for i, t in enumerate(payload.get("targets", []))}
    by_bench: dict[str, dict] = {}
    for rec in payload.get("records", []):
        if rec.get("status") != "ok":
            continue
        samples = rec.get("samples")
        if not samples and isinstance(rec.get("metrics"), dict):
            samples = [rec["metrics"]]
        if samples:
            by_bench.setdefault(rec["bench"], {})[rec["target"]] = samples

    collected = []
    for bench, by_target in by_bench.items():
        all_samples = [s for samples in by_target.values() for s in samples]
        metric = pick_metric(all_samples)
        if metric is None:
            continue
        groups = []
        for tid in sorted(by_target, key=lambda t: target_order.get(t, len(target_order))):
            values = [s[metric] for s in by_target[tid] if isinstance(s.get(metric), (int, float))]
            if values:
                groups.append((tid, values))
        if groups:
            collected.append((bench, metric, groups))
    return collected


def _summary_table(groups: list, palette: dict) -> str:
    rows = []
    for label, values in groups:
        stats = box_stats(values)
        rows.append(
            "<tr>"
            '<td><span class="dot" style="background:%s"></span>%s</td>'
            "<td>%d</td><td>%s</td><td>%s</td><td>%s</td><td>%s</td><td>%s</td>"
            "</tr>"
            % (
                palette.get(label, "#999"),
                html.escape(str(label)),
                len(values),
                fmt_num(stats["median"]),
                fmt_num(stats["low"]),
                fmt_num(stats["q1"]),
                fmt_num(stats["q3"]),
                fmt_num(stats["high"]),
            )
        )
    return (
        '<table class="summary"><thead><tr>'
        "<th>target</th><th>n</th><th>median</th><th>min</th><th>q1</th><th>q3</th><th>max</th>"
        "</tr></thead><tbody>%s</tbody></table>" % "".join(rows)
    )


def render_report(payload: dict, title: str | None = None) -> str:
    """Render the full self-contained HTML report for a bench payload."""
    title = title or payload.get("title") or "chaoticdb benchmark"
    targets = payload.get("targets", [])
    palette = {t["id"]: PALETTE[i % len(PALETTE)] for i, t in enumerate(targets)}
    titles = {b["id"]: b.get("title", "") for b in payload.get("benches", [])}
    collected = _collect(payload)

    legend = "".join(
        '<span class="key"><span class="dot" style="background:%s"></span>%s</span>'
        % (palette.get(t["id"], "#999"), html.escape(t["id"]))
        for t in targets
    )

    sections = []
    for bench, metric, groups in collected:
        subtitle = html.escape(titles.get(bench, ""))
        sections.append(
            '<section class="bench">'
            "<h2>%s</h2>"
            '<p class="sub">%s</p>'
            '<p class="metric">%s <span class="dir">(%s)</span></p>'
            '<div class="chart">%s</div>'
            "%s"
            "</section>"
            % (
                html.escape(bench),
                subtitle,
                html.escape(metric),
                direction(metric),
                svg_boxplot(groups, metric, palette),
                _summary_table(groups, palette),
            )
        )

    if not sections:
        sections.append("<p class='empty'>No successful samples: run the benchmarks first.</p>")

    skipped = [
        "%s / %s: %s" % (r.get("bench"), r.get("target"), r.get("reason", ""))
        for r in payload.get("records", [])
        if r.get("status") != "ok"
    ]
    notes = ""
    if skipped:
        items = "".join("<li>%s</li>" % html.escape(s) for s in skipped)
        notes = '<details class="skipped"><summary>%d skipped / errored</summary><ul>%s</ul></details>' % (
            len(skipped),
            items,
        )

    repeat = payload.get("repeat")
    meta_parts = [
        "%d bench%s" % (len(collected), "" if len(collected) == 1 else "es"),
        "%d target%s" % (len(targets), "" if len(targets) == 1 else "s"),
    ]
    if repeat:
        meta_parts.append("%d repeat%s per pair" % (repeat, "" if repeat == 1 else "s"))
    meta = " · ".join(meta_parts)

    return """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>%(title)s</title>
<style>
  :root { color-scheme: light; }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 32px 20px 64px; background: #f7f8fa; color: #1f2429;
         font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 840px; margin: 0 auto; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  .lede { margin: 0 0 14px; color: #5b6470; font-size: 13px; }
  .legend { display: flex; flex-wrap: wrap; gap: 14px; margin: 0 0 22px; font-size: 13px; }
  .key { display: inline-flex; align-items: center; gap: 6px; }
  .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%%; margin-right: 6px; }
  section.bench { background: #fff; border: 1px solid #e6e8eb; border-radius: 10px;
                  padding: 18px 18px 8px; margin: 0 0 18px; }
  section.bench h2 { font-size: 16px; margin: 0; }
  .sub { margin: 2px 0 6px; color: #5b6470; font-size: 12px; }
  .metric { margin: 0 0 6px; font-size: 13px; font-weight: 600; }
  .metric .dir { font-weight: 400; color: #7a828c; }
  .chart svg { width: 100%%; height: auto; display: block; }
  table.summary { width: 100%%; border-collapse: collapse; font-size: 12.5px; margin: 8px 0 14px; }
  table.summary th, table.summary td { text-align: right; padding: 5px 8px; border-bottom: 1px solid #eef0f2; }
  table.summary th:first-child, table.summary td:first-child { text-align: left; }
  table.summary th { color: #5b6470; font-weight: 600; }
  details.skipped { max-width: 840px; margin: 0 auto; color: #5b6470; font-size: 12.5px; }
  details.skipped ul { margin: 8px 0 0; padding-left: 18px; }
  .empty { color: #5b6470; }
</style></head>
<body><main>
  <h1>%(title)s</h1>
  <p class="lede">%(meta)s</p>
  <div class="legend">%(legend)s</div>
  %(sections)s
</main>
%(notes)s
</body></html>
""" % {
        "title": html.escape(title),
        "meta": html.escape(meta),
        "legend": legend,
        "sections": "".join(sections),
        "notes": notes,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("json_path", help="results JSON written by bench.py --json")
    parser.add_argument("-o", "--out", default="bench-report.html", help="output HTML path")
    parser.add_argument("--title", help="override the report title")
    args = parser.parse_args(argv)

    with open(args.json_path, encoding="utf-8") as fh:
        payload = json.load(fh)
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(render_report(payload, args.title))
    print("wrote %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
