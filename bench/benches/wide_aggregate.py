"""Wide-row aggregation: scan + sum over a table with many wide columns.

The layout matters here: a row-at-a-time scan walks past every column to reach
the summed one, while a column-major (PAX) page reads only that column's bytes.
``BENCH_WIDE_COLS`` sets the int columns, ``BENCH_WIDE_PAD_COLS`` /
``BENCH_WIDE_PAD_LEN`` the wide ``char`` padding that the summed query skips.
Set ``CHIBIDB_LAYOUT=pax`` (see ``targets/chibidb.py``) to run it on PAX tables.
The default row count keeps the table inside the 64-frame buffer pool, so the
measurement isolates scan/decoding CPU rather than page I/O.
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


class WideAggregate(Bench):
    id = "wide_aggregate"
    title = "%d sum scans over %d rows (%d int + %d char(%d))" % (
        QUERIES,
        ROWS,
        COLS,
        PAD_COLS,
        PAD_LEN,
    )

    def run(self, target: Target, env: Env):
        target.execute("create table wide (id int, %s);" % table_defs())
        for i in range(ROWS):
            target.execute("insert into wide values (%d, %s);" % (i, row_values(i)))

        last = "c%d" % (COLS - 1)
        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select sum(%s) from wide;" % last)
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = WideAggregate()
