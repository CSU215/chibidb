"""Secondary (non-key) index lookups: measures index access on a low-cardinality
column, complementing the unique-key ``point_select`` workload.
``BENCH_SECONDARY_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_SECONDARY_ROWS", "2000"))
GROUPS = 100
QUERIES = 500


class SecondaryIndex(Bench):
    id = "secondary_index"
    title = "%d non-key index lookups over %d rows / %d keys" % (QUERIES, ROWS, GROUPS)

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, g int);")
        for i in range(ROWS):
            target.execute("insert into bench values (%d, %d);" % (i, i % GROUPS))
        target.execute("create index idx_bench_g on bench (g);")

        start = time.perf_counter()
        for i in range(QUERIES):
            target.execute("select * from bench where g = %d;" % (i % GROUPS))
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = SecondaryIndex()
