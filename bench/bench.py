#!/usr/bin/env python3
"""Benchmark runner for chibidb and peer databases.

Scans ``targets/*.py`` and ``benches/*.py``, loads each with ``importlib``,
then runs every bench against every available target. A fresh scratch data
directory is created per (bench, target) run so adapters never share state.

Usage (from the repo root):

    python bench/bench.py                    # run everything
    python bench/bench.py --targets sqlite3,chibidb
    python bench/bench.py --benches point_select
    python bench/bench.py --list
    python bench/bench.py --json bench/result.json

Each bench times itself and returns a metrics dict; this script renders it.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sys
import tempfile
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from benchkit import Bench, Env, Target  # noqa: E402

TARGETS_DIR = os.path.join(HERE, "targets")
BENCHES_DIR = os.path.join(HERE, "benches")


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


def run_one(bench: Bench, target: Target, env: Env) -> dict:
    rec = {"bench": bench.id, "target": target.id}
    if not bench.enabled:
        rec.update(status="skip", reason="disabled in bench config")
        return rec
    if target.id in tuple(bench.disabled_targets):
        rec.update(status="skip", reason="disabled for target %s" % target.id)
        return rec
    try:
        ok, reason = bench.check(target, env)
    except Exception as exc:
        rec.update(status="error", reason="check() raised %s: %s" % (type(exc).__name__, exc))
        rec["trace"] = traceback.format_exc()
        return rec
    if not ok:
        rec.update(status="skip", reason=str(reason or "unsupported"))
        return rec
    try:
        target.open(env)
        metrics = bench.run(target, env)
        if not isinstance(metrics, dict):
            raise TypeError("run() must return a dict, got %s" % type(metrics).__name__)
        rec.update(status="ok", metrics=metrics)
    except Exception as exc:
        rec.update(status="error", reason="%s: %s" % (type(exc).__name__, exc))
        rec["trace"] = traceback.format_exc()
    finally:
        try:
            target.close()
        except Exception as exc:
            if rec.get("status") == "ok":
                rec.update(status="error", reason="close() raised %s: %s" % (type(exc).__name__, exc))
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
            detail = " ".join("%s=%s" % (k, fmt(v)) for k, v in rec["metrics"].items())
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
    parser.add_argument("--keep", action="store_true", help="keep scratch data directories")
    parser.add_argument("-v", "--verbose", action="store_true", help="print tracebacks on error")
    parser.add_argument("--list", action="store_true", help="list targets and benches")
    args = parser.parse_args()

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
    for bench in benches:
        for target in targets:
            rec = {"bench": bench.id, "target": target.id}
            ok, reason = status[target.id]
            if not ok:
                rec.update(status="skip", reason="target unavailable: %s" % reason)
                records.append(rec)
                continue
            data_dir = tempfile.mkdtemp(prefix="chibidb-bench-")
            env = Env(root=ROOT, bench_dir=HERE, data_dir=data_dir)
            try:
                records.append(run_one(bench, target, env))
            finally:
                if not args.keep:
                    shutil.rmtree(data_dir, ignore_errors=True)

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

    if args.json_path:
        payload = {
            "targets": [
                {"id": t.id, "title": t.title, "available": status[t.id][0], "reason": status[t.id][1]}
                for t in targets
            ],
            "records": records,
        }
        with open(args.json_path, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2)
        print("wrote %s" % args.json_path)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
