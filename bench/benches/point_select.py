"""Indexed point lookups: measures index access path latency."""

from __future__ import annotations

import time

from benchkit import Bench, Env, Target, seed_int_table

ROWS = 2000
QUERIES = 500


class PointSelect(Bench):
    id = "point_select"
    title = "%d indexed point selects over %d rows" % (QUERIES, ROWS)

    def check(self, target: Target, env: Env):
        # 临时停用本用例：改成 return False, "temporarily disabled"
        # 不在某个 target 上跑：if target.id == "duckdb": return False, "skip duckdb"
        return True, ""

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")
        seed_int_table(target, "bench", ROWS)
        target.execute("create index idx_bench_id on bench (id);")

        start = time.perf_counter()
        for i in range(QUERIES):
            target.execute("select * from bench where id = %d;" % (i % ROWS))
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = PointSelect()
