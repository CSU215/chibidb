"""chaoticdb target via the C ABI `cdylib` (in-process, no REPL round trip).

Loads the shared library built by ``cargo build`` and calls
``chaoticdb_open``/``chaoticdb_exec``/``chaoticdb_close`` through ``ctypes``, so
the benchmark measures the engine rather than a pipe. ``CHAOTICDB_MODE`` selects
the execution mode (``volcano``/``chunk``), ``CHAOTICDB_LAYOUT`` the page layout
for new tables (``row``/``pax``); both are passed to ``chaoticdb_open`` joined
with ``+``. ``CHAOTICDB_LIB`` overrides the library path.
"""

from __future__ import annotations

import ctypes
import os
import sys

from benchkit import ROOT, Env, Target


def library_path() -> str:
    override = os.environ.get("CHAOTICDB_LIB")
    if override:
        return override
    if sys.platform.startswith("win"):
        names = ("chaoticdb_ffi.dll",)
    elif sys.platform == "darwin":
        names = ("libchaoticdb_ffi.dylib",)
    else:
        names = ("libchaoticdb_ffi.so",)
    for profile in ("release", "debug"):
        for name in names:
            candidate = os.path.join(ROOT, "target", profile, name)
            if os.path.isfile(candidate):
                return candidate
    return os.path.join(ROOT, "target", "release", names[0])


def execution_mode() -> str:
    return os.environ.get("CHAOTICDB_MODE", "").strip().lower()


def page_layout() -> str:
    return os.environ.get("CHAOTICDB_LAYOUT", "").strip().lower()


def open_options() -> str:
    """Execution mode and page layout joined as ``chaoticdb_open`` options."""
    return "+".join(part for part in (execution_mode(), page_layout()) if part)


class ChaoticdbNativeTarget(Target):
    id = "chaoticdb"
    title = "chaoticdb (native cdylib)"

    def __init__(self):
        self._lib = None
        self._db = None

    def available(self):
        path = library_path()
        if not os.path.isfile(path):
            return False, "cdylib not built: %s (cargo build --release)" % path
        options = open_options()
        return True, path + (" [%s]" % options if options else "")

    def _load(self):
        lib = ctypes.CDLL(library_path())
        lib.chaoticdb_open.restype = ctypes.c_void_p
        lib.chaoticdb_open.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
        lib.chaoticdb_exec.restype = ctypes.c_int
        lib.chaoticdb_exec.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        lib.chaoticdb_close.restype = None
        lib.chaoticdb_close.argtypes = [ctypes.c_void_p]
        lib.chaoticdb_last_error.restype = ctypes.c_char_p
        return lib

    def open(self, env: Env):
        self._lib = self._load()
        options = open_options() or None
        self._db = self._lib.chaoticdb_open(
            env.data_dir.encode("utf-8"),
            options.encode("utf-8") if options else None,
        )
        if not self._db:
            raise RuntimeError("chaoticdb_open failed: %s" % self._last_error())

    def execute(self, sql: str):
        if self._lib.chaoticdb_exec(self._db, sql.encode("utf-8")) != 0:
            raise RuntimeError(self._last_error())
        return None

    def close(self):
        if self._db:
            self._lib.chaoticdb_close(self._db)
            self._db = None
        self._lib = None

    def _last_error(self) -> str:
        message = self._lib.chaoticdb_last_error()
        return message.decode("utf-8", "replace") if message else "unknown error"


TARGET = ChaoticdbNativeTarget()
