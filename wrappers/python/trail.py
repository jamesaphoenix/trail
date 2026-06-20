"""Thin Python wrapper around the `trail` CLI.

This shells out to the `trail` binary and parses its JSON. There is no native
logic here on purpose: the CLI is the single source of truth, so this stays
correct as `trail` evolves.

Binary discovery: the `TRAIL_BIN` environment variable, else `trail` on PATH.

Example
-------
    import trail

    trail.init(root="/repo")
    for folder in trail.folders("refine", agent="a1", root="/repo"):
        # ... investigate folder["path"] ...
        trail.done("refine", folder["path"], agent="a1", root="/repo")
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from typing import Iterator, Optional

# Exit codes mirrored from the CLI.
EXIT_OK = 0
EXIT_ERROR = 1
EXIT_SWEEP_COMPLETE = 3
EXIT_NONE_AVAILABLE = 4


class TrailError(RuntimeError):
    """A `trail` command exited with an error (exit code 1)."""


def _bin() -> str:
    return os.environ.get("TRAIL_BIN", "trail")


def _run(args: list[str], root: Optional[str]) -> tuple[int, dict]:
    cmd = [_bin()]
    if root:
        cmd += ["--root", root]
    cmd += args
    proc = subprocess.run(cmd, capture_output=True, text=True)
    out = proc.stdout.strip()
    data: dict = {}
    if out:
        try:
            data = json.loads(out.splitlines()[-1])
        except json.JSONDecodeError:
            data = {}
    if proc.returncode == EXIT_ERROR:
        msg = data.get("error") or proc.stderr.strip() or "trail error"
        raise TrailError(msg)
    return proc.returncode, data


def init(root: Optional[str] = None) -> dict:
    """Scan the tree and register the folder snapshot."""
    return _run(["init"], root)[1]


def claim(
    task: str,
    agent: Optional[str] = None,
    root: Optional[str] = None,
    strategy: Optional[str] = None,
    auto_sweep: bool = False,
    poll_secs: float = 2.0,
) -> Optional[dict]:
    """Claim the next folder.

    Returns the folder dict (with `path`, `score`, ...) when one is leased, or
    ``None`` when the sweep is complete. Blocks and retries while folders are
    only leased elsewhere (exit code 4).
    """
    args = ["next", "--task", task]
    if agent:
        args += ["--agent", agent]
    if strategy:
        args += ["--strategy", strategy]
    if auto_sweep:
        args += ["--auto-sweep"]
    while True:
        code, data = _run(args, root)
        if code == EXIT_OK:
            return data
        if code == EXIT_SWEEP_COMPLETE:
            return None
        if code == EXIT_NONE_AVAILABLE:
            time.sleep(poll_secs)
            continue
        raise TrailError(data.get("error", f"unexpected exit code {code}"))


def folders(
    task: str,
    agent: Optional[str] = None,
    root: Optional[str] = None,
    strategy: Optional[str] = None,
    auto_sweep: bool = False,
) -> Iterator[dict]:
    """Yield folder dicts until the sweep completes. Remember to call `done`."""
    while True:
        folder = claim(
            task, agent=agent, root=root, strategy=strategy, auto_sweep=auto_sweep
        )
        if folder is None:
            return
        yield folder


def done(task: str, path: str, agent: Optional[str] = None, root: Optional[str] = None) -> dict:
    """Mark a folder covered and append it to the task's history."""
    args = ["done", "--task", task, "--path", path]
    if agent:
        args += ["--agent", agent]
    return _run(args, root)[1]


def skip(
    task: str,
    path: str,
    agent: Optional[str] = None,
    reason: Optional[str] = None,
    root: Optional[str] = None,
) -> dict:
    """Mark a folder covered-but-skipped."""
    args = ["skip", "--task", task, "--path", path]
    if agent:
        args += ["--agent", agent]
    if reason:
        args += ["--reason", reason]
    return _run(args, root)[1]


def status(task: str, root: Optional[str] = None) -> dict:
    """Coverage snapshot for the task's latest sweep."""
    return _run(["status", "--task", task], root)[1]


def new_sweep(task: str, rescan: bool = False, root: Optional[str] = None) -> dict:
    """Open a fresh sweep for the task."""
    args = ["sweep", "new", "--task", task]
    if rescan:
        args += ["--rescan"]
    return _run(args, root)[1]
