"""Ordered top-N retrieval: measures ORDER BY ... LIMIT when an index already
provides the requested order (chaoticdb's ``OrderedIndexScan`` skip-sort path).
``BENCH_ORDER_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target, seed_int_table

ROWS = int(os.environ.get("BENCH_ORDER_ROWS", "2000"))
QUERIES = 200


class OrderByLimit(Bench):
    id = "order_by_limit"
    title = "%d ordered top-10 scans over %d rows" % (QUERIES, ROWS)

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")
        seed_int_table(target, "bench", ROWS)
        target.execute("create index idx_bench_id on bench (id);")

        start = time.perf_counter()
        for _ in range(QUERIES):
            target.execute("select id from bench order by id limit 10;")
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "queries": QUERIES, "queries/s": QUERIES / seconds}


BENCH = OrderByLimit()
