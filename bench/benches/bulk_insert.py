"""Autocommit single-row inserts: measures per-statement write latency."""

from __future__ import annotations

import time

from benchkit import Bench, Env, Target

ROWS = 2000


class BulkInsert(Bench):
    id = "bulk_insert"
    title = "insert %d rows, one statement each" % ROWS

    def check(self, target: Target, env: Env):
        # 临时停用本用例：改成 return False, "temporarily disabled"
        # 不在某个 target 上跑：if target.id == "duckdb": return False, "skip duckdb"
        return True, ""

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")
        start = time.perf_counter()
        for i in range(ROWS):
            target.execute("insert into bench values (%d, %d);" % (i, i * 3))
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "rows": ROWS, "rows/s": ROWS / seconds}


BENCH = BulkInsert()
