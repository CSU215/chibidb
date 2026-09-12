#!/usr/bin/env python3
"""End-to-end smoke test: server -> client SQL -> crash -> WAL recovery.

Usage (from the repo root, on Windows / macOS / Linux):

    python3 scripts/smoke.py     # POSIX
    python  scripts\\smoke.py     # Windows

Builds the debug binary, starts ``serve``, drives SQL through the TCP client,
kills the server without a clean flush, reopens the data directory and checks
that the committed rows survived via WAL replay. Exits non-zero on failure.

Unlike the in-process ``cargo test`` crash tests, this exercises the actual
shipped binaries across a real socket, so it catches packaging/CLI regressions.
"""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA_DIR = os.path.join(ROOT, "tmp", "smoke")


def binary() -> str:
    """Path to the debug binary Cargo just produced."""
    name = "chibidb.exe" if os.name == "nt" else "chibidb"
    return os.path.join(ROOT, "target", "debug", name)


def run(cmd, **kwargs) -> subprocess.CompletedProcess:
    kwargs.setdefault("cwd", ROOT)
    kwargs.setdefault("capture_output", True)
    kwargs.setdefault("text", True)
    kwargs.setdefault("encoding", "utf-8")
    kwargs.setdefault("errors", "replace")
    return subprocess.run(cmd, **kwargs)


def free_addr() -> str:
    """A loopback address with a free port, so a dev server cannot collide."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return "127.0.0.1:%d" % s.getsockname()[1]


def wait_for_port(addr: str, proc: subprocess.Popen, timeout: float = 30.0) -> bool:
    host, port = addr.rsplit(":", 1)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return False
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.25)
            try:
                s.connect((host, int(port)))
                return True
            except OSError:
                time.sleep(0.1)
    return False


def main() -> int:
    build = run(["cargo", "build", "-q"])
    if build.returncode != 0:
        sys.stderr.write(build.stderr)
        print("build failed", file=sys.stderr)
        return 1

    if os.path.isdir(DATA_DIR):
        shutil.rmtree(DATA_DIR)
    os.makedirs(DATA_DIR)

    # A scratch cwd keeps a repo config.toml from redirecting ports; the data
    # directory and listen address are passed explicitly instead.
    scratch = tempfile.mkdtemp(prefix="chibidb-smoke-")
    addr = free_addr()
    server_log_path = os.path.join(scratch, "server.log")
    with open(server_log_path, "w+", encoding="utf-8") as server_log:
        server = subprocess.Popen(
            [binary(), "serve", DATA_DIR, addr],
            cwd=scratch,
            stdout=server_log,
            stderr=subprocess.STDOUT,
        )
        try:
            if not wait_for_port(addr, server):
                server_log.flush()
                server_log.seek(0)
                sys.stderr.write(server_log.read())
                print("server did not start", file=sys.stderr)
                return 1

            client_sql = "\n".join(
                [
                    "create table t (id int, name char(10));",
                    "insert into t values (1, 'alice'), (2, 'bob');",
                    "create index idx on t (id);",
                    "explain select * from t where id = 2;",
                    "select * from t where id = 2;",
                    "begin;",
                    "insert into t values (3, 'carol');",
                    "commit;",
                    "exit",
                ]
            ) + "\n"
            client = run([binary(), "client", addr], input=client_sql, cwd=scratch)
            sys.stdout.write(client.stdout)
            if client.returncode != 0:
                sys.stderr.write(client.stderr)
                print("client failed", file=sys.stderr)
                return 1

            # Kill WITHOUT a clean flush: only the WAL is durable.
            server.kill()
            server.wait(timeout=10)
        finally:
            if server.poll() is None:
                server.kill()
                server.wait()

    # Reopen the directory: committed data (including the index path) must survive.
    reopen_sql = "select * from t order by id;\nexit\n"
    after = run([binary(), DATA_DIR], input=reopen_sql, cwd=scratch)
    sys.stdout.write(after.stdout)
    shutil.rmtree(scratch, ignore_errors=True)

    if "alice" in after.stdout and "carol" in after.stdout:
        print("\nSMOKE OK: committed data survived the crash via WAL")
        return 0
    print("\nSMOKE FAILED: committed data lost", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
