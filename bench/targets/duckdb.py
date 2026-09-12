"""DuckDB target. Optional dependency: reported unavailable when not installed."""

from __future__ import annotations

import os

from benchkit import Env, Target


class DuckdbTarget(Target):
    id = "duckdb"
    title = "DuckDB"

    def __init__(self):
        self._conn = None

    def available(self):
        try:
            import duckdb
        except ImportError as exc:
            return False, "not installed: %s" % exc
        return True, "duckdb %s" % duckdb.__version__

    def open(self, env: Env):
        import duckdb

        self._conn = duckdb.connect(os.path.join(env.data_dir, "bench.duckdb"))

    def execute(self, sql: str):
        cur = self._conn.execute(sql)
        return cur.fetchall()

    def close(self):
        if self._conn is not None:
            self._conn.close()
            self._conn = None


TARGET = DuckdbTarget()
