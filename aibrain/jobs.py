"""Running maintenance scripts and streaming their output to the UI.

A job is a subprocess (the MacWhisper exporter) or an in-process callable (a
reindex). Both look the same from the browser: a job id, a status, and a line
stream you can subscribe to, including lines emitted before you subscribed.
"""

from __future__ import annotations

import os
import queue
import shlex
import signal
import subprocess
import threading
import time
import uuid
from dataclasses import dataclass, field
from typing import Callable, Iterator

MAX_LINES = 4000


@dataclass
class Job:
    id: str
    name: str
    kind: str                       # "script" | "task"
    command: list[str] = field(default_factory=list)
    cwd: str = ""
    status: str = "running"         # running | done | failed | cancelled
    exit_code: int | None = None
    started: float = field(default_factory=time.time)
    finished: float | None = None
    lines: list[str] = field(default_factory=list)
    _subs: list[queue.Queue] = field(default_factory=list, repr=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False)
    _proc: subprocess.Popen | None = field(default=None, repr=False)

    # ---- stream plumbing -------------------------------------------------
    def emit(self, line: str) -> None:
        line = line.rstrip("\n")
        with self._lock:
            self.lines.append(line)
            if len(self.lines) > MAX_LINES:
                del self.lines[: len(self.lines) - MAX_LINES]
            subs = list(self._subs)
        for q in subs:
            try:
                q.put_nowait(line)
            except queue.Full:
                pass

    def subscribe(self) -> tuple[list[str], queue.Queue]:
        q: queue.Queue = queue.Queue(maxsize=2000)
        with self._lock:
            backlog = list(self.lines)
            self._subs.append(q)
        return backlog, q

    def unsubscribe(self, q: queue.Queue) -> None:
        with self._lock:
            if q in self._subs:
                self._subs.remove(q)

    def finish(self, status: str, exit_code: int | None = None) -> None:
        self.status = status
        self.exit_code = exit_code
        self.finished = time.time()
        with self._lock:
            subs = list(self._subs)
        for q in subs:
            try:
                q.put_nowait(None)   # sentinel: stream over
            except queue.Full:
                pass

    def cancel(self) -> bool:
        proc = self._proc
        if proc is None or proc.poll() is not None:
            return False
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            proc.terminate()
        self.emit("— cancelled —")
        return True

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "status": self.status,
            "exitCode": self.exit_code,
            "started": self.started,
            "finished": self.finished,
            "command": " ".join(shlex.quote(c) for c in self.command),
            "lineCount": len(self.lines),
            "tail": self.lines[-3:],
        }


class JobRunner:
    """Keeps the recent jobs around so the UI can show what last happened."""

    def __init__(self, history: int = 30):
        self.jobs: dict[str, Job] = {}
        self.order: list[str] = []
        self.history = history
        self._lock = threading.Lock()

    def _register(self, job: Job) -> Job:
        with self._lock:
            self.jobs[job.id] = job
            self.order.append(job.id)
            while len(self.order) > self.history:
                old = self.order.pop(0)
                self.jobs.pop(old, None)
        return job

    def get(self, job_id: str) -> Job | None:
        return self.jobs.get(job_id)

    def list(self) -> list[dict]:
        with self._lock:
            ids = list(reversed(self.order))
        return [self.jobs[i].to_dict() for i in ids if i in self.jobs]

    def running(self, name: str) -> Job | None:
        for jid in reversed(self.order):
            job = self.jobs.get(jid)
            if job and job.name == name and job.status == "running":
                return job
        return None

    # ---- launchers -------------------------------------------------------
    def run_command(
        self,
        name: str,
        command: list[str],
        cwd: str = "",
        env: dict[str, str] | None = None,
        on_success: Callable[[Job], None] | None = None,
    ) -> Job:
        job = self._register(Job(
            id=uuid.uuid4().hex[:12], name=name, kind="script",
            command=list(command), cwd=cwd,
        ))

        def worker() -> None:
            job.emit(f"$ {' '.join(shlex.quote(c) for c in command)}")
            if cwd:
                job.emit(f"  in {cwd}")
            merged = {**os.environ, **(env or {}), "PYTHONUNBUFFERED": "1"}
            try:
                proc = subprocess.Popen(
                    command,
                    cwd=cwd or None,
                    env=merged,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    bufsize=1,
                    start_new_session=True,
                )
            except (OSError, ValueError) as exc:
                job.emit(f"failed to start: {exc}")
                job.finish("failed", -1)
                return
            job._proc = proc
            assert proc.stdout is not None
            for line in proc.stdout:
                job.emit(line)
            code = proc.wait()
            job.emit(f"— exited with code {code} —")
            job.finish("done" if code == 0 else "failed", code)
            if code == 0 and on_success:
                try:
                    on_success(job)
                except Exception as exc:  # a follow-up must not lose the run
                    job.emit(f"post-run step failed: {exc}")

        threading.Thread(target=worker, daemon=True, name=f"job-{name}").start()
        return job

    def run_task(self, name: str, fn: Callable[[Callable[[str], None]], None]) -> Job:
        """Run a Python callable that reports progress through an emit()."""
        job = self._register(Job(id=uuid.uuid4().hex[:12], name=name, kind="task"))

        def worker() -> None:
            try:
                fn(job.emit)
                job.finish("done", 0)
            except Exception as exc:
                job.emit(f"error: {exc!r}")
                job.finish("failed", 1)

        threading.Thread(target=worker, daemon=True, name=f"task-{name}").start()
        return job

    # ---- streaming -------------------------------------------------------
    def stream(self, job: Job, keepalive: float = 15.0) -> Iterator[str]:
        """Yield lines for SSE, starting with everything already buffered."""
        backlog, q = job.subscribe()
        try:
            for line in backlog:
                yield line
            if job.status != "running":
                return
            while True:
                try:
                    line = q.get(timeout=keepalive)
                except queue.Empty:
                    yield "\x00keepalive"
                    continue
                if line is None:
                    return
                yield line
        finally:
            job.unsubscribe(q)
