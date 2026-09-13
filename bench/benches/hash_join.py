"""Two-table equi-join: measures join throughput.

Seeds two tables with matching ids and aggregates the joined rows with ``sum``
(not ``count``) so miniob, which drops the connection on ``count``, can
participate. Uses the comma-join form supported by every target.
``BENCH_JOIN_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_JOIN_ROWS", "2000"))
QUERIES = 50


class HashJoin(Bench):
    id = "hash_join"
    title = "%d equi-joins over %d x %d rows" % (QUERIES, ROWS, ROWS)

    def run(self, target: Target, env: Env):
        target.execute("create table a (id int, v int);")
        target.execute("create table b (id int, w int);")
        for i in range(ROWS):
            target.execute("insert into a values (%d, %d);" % (i, i * 3))
            target.execute("insert into b values (%d, %d);" % (i, i * 7))

        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(a.v) from a, b where a.id = b.id;")
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = HashJoin()
