"""Full-table aggregation: measures scan + aggregate throughput.

Uses ``sum`` rather than ``count(*)``: miniob's observer drops the connection
on ``count``, while ``sum`` is supported by every target.
"""

from __future__ import annotations

import time

from benchkit import Bench, Env, Target, seed_int_table

ROWS = 2000
QUERIES = 100


class Aggregate(Bench):
    id = "aggregate"
    title = "%d sum scans over %d rows" % (QUERIES, ROWS)

    def check(self, target: Target, env: Env):
        # 临时停用本用例：改成 return False, "temporarily disabled"
        # 不在某个 target 上跑：if target.id == "duckdb": return False, "skip duckdb"
        return True, ""

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")
        seed_int_table(target, "bench", ROWS)

        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(v) from bench;")
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = Aggregate()
