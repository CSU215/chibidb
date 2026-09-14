"""Shared contract for chaoticdb benchmark targets and benches.

A *target* is a database adapter (one file per database under ``targets/``).
A *bench* is a workload (one file per scenario under ``benches/``).

``bench.py`` loads each module with ``importlib`` and reads a single
module-level object named ``TARGET`` or ``BENCH``. Optional imports (duckdb,
miniob binaries, ...) must stay inside methods so a missing dependency never
breaks discovery.
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from typing import Any

BENCH_DIR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(BENCH_DIR)


@dataclass(frozen=True)
class Env:
    """Per-run environment handed to targets and benches."""

    root: str
    bench_dir: str
    data_dir: str

    @property
    def platform(self) -> str:
        return sys.platform

    @property
    def is_linux(self) -> bool:
        return sys.platform.startswith("linux")

    @property
    def is_windows(self) -> bool:
        return sys.platform.startswith("win")

    @property
    def is_macos(self) -> bool:
        return sys.platform == "darwin"


class Target:
    """A database adapter. Subclass and set ``id`` / ``title``."""

    id = "?"
    title = "?"

    def available(self) -> tuple[bool, str]:
        """Return ``(ok, reason)`` for whether this target can run here."""
        return True, ""

    def open(self, env: Env) -> None:
        """Start or connect; ``env.data_dir`` is a fresh scratch directory."""
        raise NotImplementedError

    def execute(self, sql: str) -> Any:
        """Run one SQL statement. Returned rows may be ignored by the caller."""
        raise NotImplementedError

    def close(self) -> None:
        """Stop or disconnect. Must be safe after a partial ``open``."""
        return None


class Bench:
    """A workload. Subclass and set ``id`` / ``title``.

    ``run`` times itself and returns a metrics dict (conventionally including
    ``"seconds"``); ``bench.py`` only collects and renders it.

    Simple config: set ``enabled = False`` to switch the whole bench off, or
    list target ids in ``disabled_targets`` to keep it off one adapter. For
    anything richer, use ``check``.
    """

    id = "?"
    title = "?"
    enabled = True
    disabled_targets: tuple[str, ...] = ()

    def check(self, target: Target, env: Env) -> tuple[bool, str]:
        """Return ``(ok, reason)`` for this bench against this target."""
        return True, ""

    def run(self, target: Target, env: Env) -> dict[str, Any]:
        """Execute the workload and return self-timed metrics."""
        raise NotImplementedError


def seed_int_table(target: Target, table: str, rows: int) -> None:
    """Fill ``table (id int, v int)`` one row per statement.

    Some engines (miniob) reject multi-row ``VALUES`` and have a small request
    buffer, so benchmarks share this portable single-statement seeder.
    """
    for i in range(rows):
        target.execute("insert into %s values (%d, %d);" % (table, i, i * 3))
