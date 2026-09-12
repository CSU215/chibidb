"""SQLite target using the Python standard library."""

from __future__ import annotations

import os
import sqlite3

from benchkit import Env, Target


class Sqlite3Target(Target):
    id = "sqlite3"
    title = "SQLite (stdlib sqlite3)"

    def __init__(self):
        self._conn = None

    def available(self):
        return True, "sqlite3 %s" % sqlite3.sqlite_version

    def open(self, env: Env):
        self._conn = sqlite3.connect(os.path.join(env.data_dir, "bench.sqlite3"))
        self._conn.isolation_level = None

    def execute(self, sql: str):
        cur = self._conn.execute(sql)
        return cur.fetchall() if cur.description else None

    def close(self):
        if self._conn is not None:
            self._conn.close()
            self._conn = None


TARGET = Sqlite3Target()
