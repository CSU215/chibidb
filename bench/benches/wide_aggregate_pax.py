"""Wide aggregation on a PAX (column-major) chibidb table.

Same workload as ``wide_aggregate`` but the table is created with
``page_layout = pax``, so the scan reads only the summed column and never the
wide ``char`` padding. chibidb-only: the clause is not valid SQL elsewhere.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_AGG_ROWS", "800"))
COLS = int(os.environ.get("BENCH_WIDE_COLS", "8"))
PAD_COLS = int(os.environ.get("BENCH_WIDE_PAD_COLS", "4"))
PAD_LEN = int(os.environ.get("BENCH_WIDE_PAD_LEN", "64"))
QUERIES = 100


def table_defs():
    ints = ", ".join("c%d int" % i for i in range(COLS))
    pads = ", ".join("pad%d char(%d)" % (i, PAD_LEN) for i in range(PAD_COLS))
    return "%s, %s" % (ints, pads)


def row_values(i):
    ints = ", ".join(str(i * (k + 1)) for k in range(COLS))
    pads = ", ".join("'%s'" % ("p" * PAD_LEN) for _ in range(PAD_COLS))
    return "%s, %s" % (ints, pads)


class WideAggregatePax(Bench):
    id = "wide_aggregate_pax"
    title = "%d sum scans over %d rows (%d int + %d char(%d)) (pax)" % (
        QUERIES,
        ROWS,
        COLS,
        PAD_COLS,
        PAD_LEN,
    )
    disabled_targets = ("sqlite3", "duckdb", "miniob")

    def run(self, target: Target, env: Env):
        target.execute("create table wide (id int, %s) page_layout = pax;" % table_defs())
        for i in range(ROWS):
            target.execute("insert into wide values (%d, %s);" % (i, row_values(i)))

        last = "c%d" % (COLS - 1)
        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(%s) from wide;" % last)
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = WideAggregatePax()
