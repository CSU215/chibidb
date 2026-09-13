"""Grouped aggregation: measures GROUP BY throughput over many groups.

Uses ``sum`` rather than ``count(*)`` because miniob's observer drops the
connection on ``count`` (same rationale as the aggregate bench).
``BENCH_GROUP_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_GROUP_ROWS", "2000"))
GROUPS = 50
QUERIES = 100


class GroupBy(Bench):
    id = "group_by"
    title = "%d group-by scans over %d rows / %d groups" % (QUERIES, ROWS, GROUPS)

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, g int, v int);")
        for i in range(ROWS):
            target.execute("insert into bench values (%d, %d, %d);" % (i, i % GROUPS, i * 3))

        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select g, sum(v) from bench group by g;")
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = GroupBy()
