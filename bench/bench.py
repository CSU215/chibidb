#!/usr/bin/env python3
"""Benchmark runner for chaoticdb and peer databases.

Scans ``targets/*.py`` and ``benches/*.py``, loads each with ``importlib``,
then runs every bench against every available target. A fresh scratch data
directory is created per (bench, target) run so adapters never share state.

Usage (from the repo root):

    python bench/bench.py                    # run everything
    python bench/bench.py --targets sqlite3,chaoticdb
    python bench/bench.py --benches point_select
    python bench/bench.py --list
    python bench/bench.py --json bench/result.json
    python bench/bench.py --repeat 5 --plot bench/report.html
    python bench/bench.py --no-progress      # silence the progress bar

A progress bar on stderr tracks each (bench, target) step; it degrades to one
plain line per step when stderr is not a TTY. Each bench times itself and
returns a metrics dict; this script renders it.

Every (bench, target) pair is run ``--repeat`` times (default 5) so run-to-run
variance is visible: records carry the per-run ``samples`` alongside a median
``metrics`` aggregate, and ``--plot`` turns that spread into a box-plot HTML
report (see ``bench/plot.py``).
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import statistics
import sys
import tempfile
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from benchkit import Bench, Env, Target  # noqa: E402

TARGETS_DIR = os.path.join(HERE, "targets")
BENCHES_DIR = os.path.join(HERE, "benches")


class Progress:
    """Dependency-free progress bar for the (bench, target) run loop.

    Renders to stderr so the results table on stdout stays clean. When the
    stream is not a TTY (CI, redirected output) it falls back to one plain line
    per step, and ``enabled=False`` (``--no-progress``) silences it entirely.
    """

    WIDTH = 24

    def __init__(self, total: int, enabled: bool = True, stream=None):
        self.total = max(1, total)
        self.done = 0
        self.enabled = enabled
        self.stream = stream if stream is not None else sys.stderr
        self.interactive = bool(enabled and self.stream.isatty())
        self.start = time.monotonic()
        self._last = 0

    def update(self, label: str) -> None:
        if not self.enabled:
            return
        self.done += 1
        self._draw(label)

    def set_label(self, label: str) -> None:
        """Rewrite the current line's label without advancing the counter.

        Used to show which repeat is running; a no-op outside a TTY so the
        fallback never spams one line per sample.
        """
        if self.interactive:
            self._draw(label)

    def _draw(self, label: str) -> None:
        elapsed = time.monotonic() - self.start
        ratio = self.done / self.total
        filled = int(self.WIDTH * ratio)
        bar = "#" * filled + "-" * (self.WIDTH - filled)
        eta = elapsed / self.done * (self.total - self.done) if self.done else 0.0
        text = "[%s] %d/%d %3d%%  %s  %.1fs ETA %.1fs" % (
            bar,
            self.done,
            self.total,
            int(ratio * 100),
            label,
            elapsed,
            eta,
        )
        if self.interactive:
            pad = " " * max(0, self._last - len(text))
            self.stream.write("\r" + text + pad)
            self.stream.flush()
            self._last = len(text)
        else:
            self.stream.write(text + "\n")
            self.stream.flush()

    def finish(self) -> None:
        if self.enabled and self.interactive:
            self.stream.write("\n")
            self.stream.flush()


def load_objects(directory: str, attr: str, base: type) -> list:
    """Import every ``*.py`` in ``directory`` and return its ``attr`` object."""
    objects = []
    for name in sorted(os.listdir(directory)):
        if name.startswith("_") or not name.endswith(".py"):
            continue
        path = os.path.join(directory, name)
        mod_name = "bench_%s_%s" % (attr.lower(), name[:-3])
        spec = importlib.util.spec_from_file_location(mod_name, path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        obj = getattr(module, attr, None)
        if obj is None:
            raise SystemExit("%s: expected a module-level %s" % (path, attr))
        if isinstance(obj, type):
            obj = obj()
        if not isinstance(obj, base):
            raise SystemExit("%s: %s must subclass %s" % (path, attr, base.__name__))
        objects.append(obj)
    return objects


def select(objects: list, ids: str | None) -> list:
    if not ids:
        return objects
    wanted = [part.strip() for part in ids.split(",") if part.strip()]
    by_id = {obj.id: obj for obj in objects}
    for missing in (w for w in wanted if w not in by_id):
        raise SystemExit("unknown id: %s" % missing)
    return [by_id[w] for w in wanted]


def describe_target(target: Target) -> tuple[bool, str]:
    try:
        ok, reason = target.available()
    except Exception as exc:  # availability probing must never crash the run
        return False, "available() raised %s: %s" % (type(exc).__name__, exc)
    return bool(ok), str(reason or "")


def attempt(bench: Bench, target: Target, env: Env) -> tuple[dict | None, dict | None]:
    """One open/run/close cycle. Returns ``(metrics, error)``; one is ``None``."""
    error = None
    metrics = None
    try:
        target.open(env)
        metrics = bench.run(target, env)
        if not isinstance(metrics, dict):
            raise TypeError("run() must return a dict, got %s" % type(metrics).__name__)
    except Exception as exc:
        error = {"reason": "%s: %s" % (type(exc).__name__, exc), "trace": traceback.format_exc()}
        return None, error
    finally:
        try:
            target.close()
        except Exception as exc:
            if error is None:
                error = {"reason": "close() raised %s: %s" % (type(exc).__name__, exc)}
                metrics = None
    return metrics, error


def aggregate(samples: list) -> dict:
    """Median of every metric across samples; non-numeric keys keep their last value."""
    metrics = {}
    for key in samples[-1]:
        values = [s.get(key) for s in samples]
        if all(isinstance(v, (int, float)) and not isinstance(v, bool) for v in values):
            metrics[key] = statistics.median(values)
        else:
            metrics[key] = values[-1]
    return metrics


def run_pair(bench: Bench, target: Target, repeat: int, progress: Progress, keep: bool) -> dict:
    """Run one (bench, target) pair ``repeat`` times over fresh scratch dirs."""
    rec = {"bench": bench.id, "target": target.id}
    if not bench.enabled:
        rec.update(status="skip", reason="disabled in bench config")
        return rec
    if target.id in tuple(bench.disabled_targets):
        rec.update(status="skip", reason="disabled for target %s" % target.id)
        return rec

    samples = []
    first_error = None
    for i in range(repeat):
        data_dir = tempfile.mkdtemp(prefix="chaoticdb-bench-")
        env = Env(root=ROOT, bench_dir=HERE, data_dir=data_dir)
        try:
            if i == 0:
                try:
                    ok, reason = bench.check(target, env)
                except Exception as exc:
                    rec.update(status="error", reason="check() raised %s: %s" % (type(exc).__name__, exc))
                    rec["trace"] = traceback.format_exc()
                    return rec
                if not ok:
                    rec.update(status="skip", reason=str(reason or "unsupported"))
                    return rec
            progress.set_label("%s / %s  sample %d/%d" % (bench.id, target.id, i + 1, repeat))
            metrics, error = attempt(bench, target, env)
            if error is None:
                samples.append(metrics)
            elif first_error is None:
                first_error = error
        finally:
            if not keep:
                shutil.rmtree(data_dir, ignore_errors=True)

    if not samples:
        rec.update(status="error", reason=first_error["reason"] if first_error else "no samples")
        if first_error and first_error.get("trace"):
            rec["trace"] = first_error["trace"]
        return rec
    rec.update(status="ok", metrics=aggregate(samples), samples=samples, repeat=repeat)
    if first_error is not None:
        rec["failed_samples"] = repeat - len(samples)
        rec["reason"] = first_error["reason"]
    return rec


def fmt(value) -> str:
    if isinstance(value, float):
        return "%.6g" % value
    return str(value)


def render(records: list) -> None:
    headers = ("target", "bench", "status", "metrics")
    rows = []
    for rec in records:
        if rec["status"] == "ok":
            samples = rec.get("samples") or [rec["metrics"]]
            prefix = "n=%d  " % len(samples) if len(samples) > 1 else ""
            detail = prefix + " ".join("%s=%s" % (k, fmt(v)) for k, v in rec["metrics"].items())
        else:
            detail = rec.get("reason", "")
        rows.append((rec["target"], rec["bench"], rec["status"], detail))

    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))
    line = "  ".join(h.ljust(widths[i]) for i, h in enumerate(headers))
    print(line)
    print("-" * len(line))
    for row in rows:
        print("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--targets", help="comma-separated target ids")
    parser.add_argument("--benches", help="comma-separated bench ids")
    parser.add_argument("--json", dest="json_path", help="write structured results here")
    parser.add_argument(
        "--plot",
        dest="plot_path",
        help="write a box-plot HTML report here (see bench/plot.py)",
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=5,
        help="times to run each (bench, target) pair, for the box-plot spread (default 5)",
    )
    parser.add_argument("--keep", action="store_true", help="keep scratch data directories")
    parser.add_argument("--no-progress", action="store_true", help="disable the progress bar")
    parser.add_argument("-v", "--verbose", action="store_true", help="print tracebacks on error")
    parser.add_argument("--list", action="store_true", help="list targets and benches")
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")

    targets = select(load_objects(TARGETS_DIR, "TARGET", Target), args.targets)
    benches = select(load_objects(BENCHES_DIR, "BENCH", Bench), args.benches)

    status = {target.id: describe_target(target) for target in targets}

    if args.list:
        print("targets:")
        for target in targets:
            ok, reason = status[target.id]
            print("  %-10s %-12s %s" % (target.id, "available" if ok else "unavailable", reason))
        print("benches:")
        for bench in benches:
            state = "enabled" if bench.enabled else "disabled"
            if bench.disabled_targets:
                state += " (skip: %s)" % ",".join(bench.disabled_targets)
            print("  %-14s %-12s %s" % (bench.id, state, bench.title))
        return 0

    print("targets:")
    for target in targets:
        ok, reason = status[target.id]
        print("  %-10s %-12s %s" % (target.id, "available" if ok else "unavailable", reason))
    print()

    records = []
    progress = Progress(len(benches) * len(targets), enabled=not args.no_progress)
    for bench in benches:
        for target in targets:
            progress.update("%s / %s" % (bench.id, target.id))
            ok, reason = status[target.id]
            if not ok:
                records.append({"bench": bench.id, "target": target.id, "status": "skip",
                                "reason": "target unavailable: %s" % reason})
                continue
            records.append(run_pair(bench, target, args.repeat, progress, args.keep))
    progress.finish()

    print()
    render(records)

    ok = sum(1 for r in records if r["status"] == "ok")
    skipped = sum(1 for r in records if r["status"] == "skip")
    failed = sum(1 for r in records if r["status"] == "error")
    print()
    print("summary: %d ok, %d skipped, %d error" % (ok, skipped, failed))

    if args.verbose:
        for rec in records:
            if rec["status"] == "error" and rec.get("trace"):
                print("\n[%s / %s]\n%s" % (rec["bench"], rec["target"], rec["trace"]))

    payload = {
        "title": "chaoticdb benchmark",
        "repeat": args.repeat,
        "targets": [
            {"id": t.id, "title": t.title, "available": status[t.id][0], "reason": status[t.id][1]}
            for t in targets
        ],
        "benches": [{"id": b.id, "title": b.title} for b in benches],
        "records": records,
    }

    if args.json_path:
        with open(args.json_path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2)
        print("wrote %s" % args.json_path)

    if args.plot_path:
        from plot import render_report  # sibling module next to this file

        with open(args.plot_path, "w", encoding="utf-8") as fh:
            fh.write(render_report(payload))
        print("wrote %s" % args.plot_path)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
