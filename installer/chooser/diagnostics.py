"""Journal diagnostics with optional, bounded console mirroring."""

import errno
import json
import os
from pathlib import Path
import subprocess
import sys
import termios
import time

MAX_BYTES = 4096
BUDGET = 0.2
CREDENTIAL = Path("/run/credentials/@system/ilex.chooser.serial-test")


def attempt(path, mode, payload):
    # Runs in a disposable worker: even a driver's last-close drain must not
    # delay the installer. Never change the real UART's termios or flush it.
    held = []
    try:
        if mode == "unavailable":
            path = "/run/ilex-installer/no-such-console"
        elif mode == "backpressure":
            master, slave = os.openpty()
            held = [master, slave]
            termios.tcflow(slave, termios.TCOOFF)
            path = os.ttyname(slave)
        fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK | os.O_NOCTTY | os.O_CLOEXEC)
        try:
            # One attempt only: drop a partial write; never retry, flush or drain.
            complete = os.write(fd, payload) == len(payload)
        finally:
            os.close(fd)
        return 0 if complete else 13
    except OSError as error:
        if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
            return 10
        return 11 if error.errno in (errno.ENOENT, errno.ENODEV, errno.ENXIO) else 14
    finally:
        for fd in held:
            os.close(fd)


def mirror(path, mode, message):
    payload = (message[:MAX_BYTES] + "\n").encode("utf-8", errors="replace")[:MAX_BYTES]
    started = time.monotonic()
    try:
        worker = subprocess.Popen(["/usr/bin/python3", "-I", "-S", __file__, path, mode,
                                   payload.decode("utf-8", errors="ignore")],
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL)
        try:
            status = worker.wait(timeout=BUDGET)
        except subprocess.TimeoutExpired:
            worker.kill()
            # Bound reaping too. A pathological unkillable kernel task cannot
            # make an optional diagnostic wait indefinitely.
            try:
                worker.wait(timeout=BUDGET)
            except subprocess.TimeoutExpired:
                pass
            status = 12
    except OSError:
        status = 11
    return {"status": {0: "written", 10: "backpressure", 11: "unavailable",
                       13: "partial", 14: "error"}.get(status, "budget"),
            "seconds": time.monotonic() - started}


def emit(message, *, consoles=("/dev/ttyS0",), journal=sys.stderr):
    print(message, file=journal, flush=True)
    mode = CREDENTIAL.read_text().strip() if CREDENTIAL.is_file() else ""
    mode = mode if mode in ("unavailable", "backpressure") else ""
    for path in consoles:
        result = mirror(path, mode if path == "/dev/ttyS0" else "", message)
        if mode and path == "/dev/ttyS0":
            # Fixed public e2e fault. Observe through a separate UART, never
            # through the unavailable/stopped sink being exercised.
            audit = json.dumps(result | {"mode": mode, "message": message.split(" ", 1)[0]})
            mirror("/dev/ttyS1", "", message + "\nILEX-CHOOSER-SERIAL " + audit)


if __name__ == "__main__":
    sys.exit(attempt(sys.argv[1], sys.argv[2], sys.argv[3].encode("utf-8")))
