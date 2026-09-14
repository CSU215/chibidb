"""OceanBase miniob target (Linux only).

Starts the ``observer`` server with a unix socket inside the scratch data
directory and speaks its CLI protocol: send ``sql + NUL``, read until a chunk
ends with NUL. Gated behind Linux and the presence of a built observer, so on
Windows it simply reports unavailable.

Build miniob first (``tmp/miniob`` by default, override with ``MINIOB_DIR``):
``cmake -B build && cmake --build build`` under the miniob checkout.
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import time

from benchkit import ROOT, Env, Target

MINIOB_DIR = os.environ.get("MINIOB_DIR") or os.path.join(ROOT, "tmp", "miniob")


def observer_path() -> str:
    return os.path.join(MINIOB_DIR, "build", "bin", "observer")


def config_path() -> str:
    return os.path.join(MINIOB_DIR, "etc", "observer.ini")


class MiniobTarget(Target):
    id = "miniob"
    title = "OceanBase miniob (observer over unix socket)"

    def __init__(self):
        self._proc = None
        self._sock = None
        self._socket_path = None

    def available(self):
        if not sys.platform.startswith("linux"):
            return False, "linux only (current: %s)" % sys.platform
        if not os.path.isfile(observer_path()):
            return False, "observer not built: %s" % observer_path()
        if not os.path.isfile(config_path()):
            return False, "config missing: %s" % config_path()
        return True, observer_path()

    def open(self, env: Env):
        self._socket_path = os.path.join(env.data_dir, "miniob.sock")
        self._proc = subprocess.Popen(
            [observer_path(), "-f", config_path(), "-s", self._socket_path],
            cwd=env.data_dir,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                raise RuntimeError("observer exited with code %s" % self._proc.returncode)
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(self._socket_path)
                self._sock = sock
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("observer did not open %s" % self._socket_path)

    def execute(self, sql: str):
        self._sock.sendall(sql.encode("utf-8") + b"\x00")
        chunks = []
        while True:
            data = self._sock.recv(8192)
            if not data:
                raise RuntimeError("miniob closed the connection")
            chunks.append(data)
            if data[-1] == 0:
                break
        return b"".join(chunks).decode("utf-8", "replace").replace("\x00", "").strip()

    def close(self):
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None
        if self._proc is not None:
            if self._proc.poll() is None:
                self._proc.terminate()
                try:
                    self._proc.wait(timeout=10)
                except Exception:
                    self._proc.kill()
                    self._proc.wait()
            self._proc = None


TARGET = MiniobTarget()
