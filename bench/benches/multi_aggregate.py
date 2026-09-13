"""Multi-column global aggregation: ``sum`` over several columns at once.

Exercises the projected scan: the engine streams only the summed columns
instead of rebuilding each row. ``BENCH_AGG_ROWS`` overrides the row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_AGG_ROWS", "2000"))
QUERIES = 100


class MultiAggregate(Bench):
    id = "multi_aggregate"
    title = "%d multi-column sum scans over %d rows" % (QUERIES, ROWS)

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, a int, b int, c int);")
        for i in range(ROWS):
            target.execute(
                "insert into bench values (%d, %d, %d, %d);" % (i, i, i * 2, i * 3)
            )

        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(a), sum(b), sum(c) from bench;")
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = MultiAggregate()
