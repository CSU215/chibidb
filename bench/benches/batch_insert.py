"""Batched multi-row inserts: measures insert throughput when each statement
carries many rows, contrasting with the single-row ``bulk_insert`` workload.

miniob rejects multi-row ``VALUES`` lists, so it is skipped via ``check``.
``BENCH_BATCH_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target

ROWS = int(os.environ.get("BENCH_BATCH_ROWS", "2000"))
BATCH = 100


class BatchInsert(Bench):
    id = "batch_insert"
    title = "insert %d rows in batches of %d" % (ROWS, BATCH)

    def check(self, target: Target, env: Env):
        if target.id == "miniob":
            return False, "miniob rejects multi-row VALUES"
        return True, ""

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")

        start = time.perf_counter()
        for base in range(0, ROWS, BATCH):
            values = ", ".join(
                "(%d, %d)" % (i, i * 3) for i in range(base, min(base + BATCH, ROWS))
            )
            target.execute("insert into bench values %s;" % values)
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "rows": ROWS, "rows/s": ROWS / seconds}


BENCH = BatchInsert()
