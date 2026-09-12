"""chibidb target driving the shipped REPL binary over stdin/stdout.

Each statement is written as one line and we read until the next ``db> ``
prompt, so one round trip == one parsed statement. The child runs with its
working directory set to the scratch data dir, so a repo ``config.toml``
cannot redirect the engine or enable auth. Set ``CHIBIDB_MODE=chunk`` (or
``volcano``) to write that execution mode into the scratch dir's config.
"""

from __future__ import annotations

import os
import subprocess
import sys

from benchkit import ROOT, Env, Target

PROMPT = b"db> "


def execution_mode() -> str:
    return os.environ.get("CHIBIDB_MODE", "").strip().lower()


def binary_path() -> str:
    override = os.environ.get("CHIBIDB_BIN")
    if override:
        return override
    name = "chibidb.exe" if sys.platform.startswith("win") else "chibidb"
    for profile in ("debug", "release"):
        candidate = os.path.join(ROOT, "target", profile, name)
        if os.path.isfile(candidate):
            return candidate
    return os.path.join(ROOT, "target", "debug", name)


class ChibidbReplTarget(Target):
    id = "chibidb-repl"
    title = "chibidb (REPL subprocess, stdio)"

    def __init__(self):
        self._proc = None
        self._buffer = b""

    def available(self):
        path = binary_path()
        if not os.path.isfile(path):
            return False, "binary not built: %s (run cargo build)" % path
        mode = execution_mode()
        return True, path + (" [mode=%s]" % mode if mode else "")

    def open(self, env: Env):
        mode = execution_mode()
        if mode:
            with open(os.path.join(env.data_dir, "config.toml"), "w", encoding="utf-8") as fh:
                fh.write('[execution]\nmode = "%s"\n' % mode)
        self._proc = subprocess.Popen(
            [binary_path(), env.data_dir],
            cwd=env.data_dir,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self._buffer = b""
        self._read_prompt()

    def execute(self, sql: str):
        self._proc.stdin.write(sql.encode("utf-8") + b"\n")
        self._proc.stdin.flush()
        return self._read_prompt().decode("utf-8", "replace")

    def close(self):
        if self._proc is not None:
            if self._proc.poll() is None:
                try:
                    self._proc.stdin.write(b"exit\n")
                    self._proc.stdin.flush()
                    self._proc.wait(timeout=10)
                except Exception:
                    self._proc.kill()
                    self._proc.wait()
            self._proc = None
        self._buffer = b""

    def _read_prompt(self) -> bytes:
        while PROMPT not in self._buffer:
            chunk = os.read(self._proc.stdout.fileno(), 4096)
            if not chunk:
                raise RuntimeError("chibidb REPL exited before the next prompt")
            self._buffer += chunk
        end = self._buffer.index(PROMPT)
        output, self._buffer = self._buffer[:end], self._buffer[end + len(PROMPT):]
        return output


TARGET = ChibidbReplTarget()
