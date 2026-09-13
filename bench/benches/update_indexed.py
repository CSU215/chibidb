"""Indexed single-row updates: measures UPDATE latency through an index.
``BENCH_UPDATE_ROWS`` overrides the seeded row count.
"""

from __future__ import annotations

import os
import time

from benchkit import Bench, Env, Target, seed_int_table

ROWS = int(os.environ.get("BENCH_UPDATE_ROWS", "2000"))
UPDATES = 500


class UpdateIndexed(Bench):
    id = "update_indexed"
    title = "%d indexed updates over %d rows" % (UPDATES, ROWS)

    def run(self, target: Target, env: Env):
        target.execute("create table bench (id int, v int);")
        seed_int_table(target, "bench", ROWS)
        target.execute("create index idx_bench_id on bench (id);")

        start = time.perf_counter()
        for i in range(UPDATES):
            target.execute("update bench set v = %d where id = %d;" % (i, i % ROWS))
        seconds = time.perf_counter() - start
        return {"seconds": seconds, "updates": UPDATES, "updates/s": UPDATES / seconds}


BENCH = UpdateIndexed()
