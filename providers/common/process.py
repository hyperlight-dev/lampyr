import os
from pathlib import Path
import selectors
import signal
import subprocess
import time


class ProviderError(Exception):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def fail(code, message):
    raise ProviderError(code, message)


def stop_process(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def bounded_run(command, *, env=None, timeout, stdout_limit, stderr_limit):
    process = subprocess.Popen(
        command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + timeout
    selector = selectors.DefaultSelector()
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    limits = {"stdout": stdout_limit, "stderr": stderr_limit}
    for name in buffers:
        pipe = getattr(process, name)
        os.set_blocking(pipe.fileno(), False)
        selector.register(pipe, selectors.EVENT_READ, name)
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                stop_process(process)
                raise subprocess.TimeoutExpired(
                    command, timeout, output=bytes(buffers["stdout"]),
                    stderr=bytes(buffers["stderr"]),
                )
            for key, _ in selector.select(min(0.1, remaining)):
                chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                output = buffers[key.data]
                if len(output) + len(chunk) > limits[key.data]:
                    stop_process(process)
                    fail("output-limit", f"Packaged {Path(command[0]).name} output exceeded its limit")
                output.extend(chunk)
        remaining = max(0, deadline - time.monotonic())
        try:
            returncode = process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            stop_process(process)
            raise subprocess.TimeoutExpired(
                command, timeout, output=bytes(buffers["stdout"]),
                stderr=bytes(buffers["stderr"]),
            )
    finally:
        selector.close()
        for name in buffers:
            getattr(process, name).close()
    return subprocess.CompletedProcess(
        command, returncode,
        bytes(buffers["stdout"]).decode("utf-8", errors="replace"),
        bytes(buffers["stderr"]).decode("utf-8", errors="replace"),
    )
