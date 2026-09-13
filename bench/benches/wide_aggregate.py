"""Wide-row aggregation: scan + sum over a table with many columns.

The layout matters here: a row-at-a-time scan walks past every column to reach
the summed one, while a column-major (PAX) page reads only that column's bytes.
``BENCH_WIDE_COLS`` overrides the column count, ``BENCH_AGG_ROWS`` the rows.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_AGG_ROWS", "2000"))
COLS = int(os.environ.get("BENCH_WIDE_COLS", "16"))
QUERIES = 100


class WideAggregate(Bench):
    id = "wide_aggregate"
    title = "%d sum scans over %d rows x %d columns" % (QUERIES, ROWS, COLS)

    def run(self, target: Target, env: Env):
        cols = ", ".join("c%d int" % i for i in range(COLS))
        target.execute("create table wide (id int, %s);" % cols)
        for i in range(ROWS):
            values = ", ".join(str(i * (k + 1)) for k in range(COLS))
            target.execute("insert into wide values (%d, %s);" % (i, values))

        last = "c%d" % (COLS - 1)
        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(%s) from wide;" % last)
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = WideAggregate()
