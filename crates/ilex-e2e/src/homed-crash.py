"""Guest-only deterministic power-loss gates for Fedora's real homed worker.

No replacement homectl/homework, injected failure return, or SELinux change.
ptrace stops the real BTRFS_IOC_RESIZE at entry for growth (lower layers have
grown) and successful exit for shrink (lower layers have not shrunk yet).
Only public geometry and the resize ioctl's size argument leave the tracer.
"""
import ctypes
import json
import os
from pathlib import Path
import re
import signal
import struct
import subprocess as sp
import time

STATE = Path('/run/ilex-homed-crash')
PTRACE_CONT, PTRACE_PEEKDATA, PTRACE_SYSCALL = 7, 2, 24
PTRACE_SEIZE, PTRACE_GETEVENTMSG, PTRACE_GET_SYSCALL_INFO = 0x4206, 0x4201, 0x420e
# TRACESYSGOOD | TRACEFORK | TRACEVFORK | TRACECLONE | TRACEEXEC.
OPTIONS = 1 | 2 | 4 | 8 | 16
WALL = 0x40000000
# Linux x86-64 UAPI: _IOW(0x94, 3, struct btrfs_ioctl_vol_args[4096]).
BTRFS_IOC_RESIZE = 0x50009403
libc = ctypes.CDLL(None, use_errno=True)
libc.ptrace.restype = ctypes.c_long
libc.ptrace.argtypes = [ctypes.c_uint, ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p]


def ptrace(request, pid, address=0, data=0):
    ctypes.set_errno(0)
    result = libc.ptrace(request, pid, address, data)
    error = ctypes.get_errno()
    if result == -1 and error:
        raise OSError(error, os.strerror(error))
    return result


def write(name, value):
    temporary = STATE / (name + '.new')
    temporary.write_text(json.dumps(value))
    temporary.replace(STATE / name)


def failure(error, password):
    write('error.json', {'error': str(error).replace(password, '[redacted]')})


def start(name, password, direction, terminal, sizes):
    assert direction in ('grow', 'shrink')
    assert os.uname().machine == 'x86_64', 'resize syscall gate requires Linux x86-64'
    STATE.mkdir(mode=0o700)
    pid = os.fork()
    if pid:
        return {'tracer_pid': pid, 'direction': direction}
    os.setsid()
    # Do not retain the SSH reply pipe or send terminal transcripts to logs.
    with open('/dev/null', 'r+b', buffering=0) as sink:
        for fd in (0, 1, 2):
            os.dup2(sink.fileno(), fd)
    try:
        trace_resize(name, password, direction, terminal, sizes)
    except Exception as error:
        failure(error, password)
    os._exit(0)


def trace_resize(name, password, direction, terminal, sizes):
    manager = int(sp.check_output(['systemctl', 'show', 'systemd-homed', '-p', 'MainPID', '--value']))
    assert manager > 1
    # Arm before homectl can ask the manager to fork the resize worker.
    ptrace(PTRACE_SEIZE, manager, 0, OPTIONS)
    client = os.fork()
    if client == 0:
        try:
            target = '3G' if direction == 'grow' else '2G'
            terminal(['/usr/bin/homectl', 'resize', name, target],
                     [(r'password[^\r\n]*:', password)], optional=True)
            write('completed.json', {'completed_before_reset': True})
        except Exception as error:
            failure(error, password)
        os._exit(0)
    tracked = {manager}
    pending = {}
    gated = None
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        pid, status = os.waitpid(-1, WALL | os.WNOHANG)
        if not pid:
            time.sleep(0.001)
            continue
        if os.WIFEXITED(status) or os.WIFSIGNALED(status):
            tracked.discard(pid)
            pending.pop(pid, None)
            if pid in (manager, client) and gated is None:
                raise RuntimeError('homed manager/resize client exited before the syscall gate')
            continue
        assert os.WIFSTOPPED(status)
        event = status >> 16
        stopped = os.WSTOPSIG(status)
        if event in (1, 2, 3):
            child = ctypes.c_ulong()
            ptrace(PTRACE_GETEVENTMSG, pid, 0, ctypes.byref(child))
            tracked.add(child.value)
        if stopped == signal.SIGTRAP | 0x80:
            info = ctypes.create_string_buffer(128)
            ptrace(PTRACE_GET_SYSCALL_INFO, pid, len(info), ctypes.byref(info))
            operation = info.raw[0]
            if operation == 1:
                number, fd, ioctl, address = struct.unpack_from('<QQQQ', info.raw, 24)
                if number == 16 and ioctl == BTRFS_IOC_RESIZE:
                    assert Path(f'/proc/{pid}/exe').resolve().name == 'systemd-homework'
                    assert os.stat(f'/proc/{pid}/fd/{fd}').st_dev == os.stat('/home/' + name).st_dev
                    # vol_args begins with an s64 fd, followed by the public
                    # resize size string. Never inspect other syscall buffers.
                    words = [ptrace(PTRACE_PEEKDATA, pid, address + 8 + i) for i in range(0, 64, 8)]
                    argument = b''.join((word & ((1 << 64) - 1)).to_bytes(8, 'little') for word in words)
                    argument = argument.split(b'\0')[0].decode('ascii')
                    assert re.fullmatch(r'(?:\d+:)?(?:\d+|max)', argument), 'unexpected resize size argument'
                    pending[pid] = argument
                    if direction == 'grow':
                        gated = pid
            elif operation == 2 and pid in pending:
                result = struct.unpack_from('<q', info.raw, 24)[0]
                assert result == 0, 'btrfs shrink ioctl failed before the crash gate'
                if direction == 'shrink':
                    gated = pid
            if gated == pid:
                write('gate.json', {
                    'direction': direction,
                    'boundary': 'BTRFS_IOC_RESIZE entry' if direction == 'grow' else 'BTRFS_IOC_RESIZE successful exit',
                    'worker_pid': pid,
                    'worker_state': next(line for line in Path(f'/proc/{pid}/status').read_text().splitlines() if line.startswith('State:')),
                    'resize_argument': pending[pid],
                    # Do not sync at the fault point: that would change the
                    # power-loss experiment. Query the live btrfs device size.
                    'sizes': sizes(name, flush=False),
                    'resize_client_completed': (STATE / 'completed.json').exists(),
                })
                # Keep this worker stopped until the host resets the VM;
                # continue servicing other traced children/manager events.
                continue
            if operation == 2:
                pending.pop(pid, None)
        delivered = 0 if event or stopped in (signal.SIGTRAP, signal.SIGSTOP, signal.SIGTRAP | 0x80) else stopped
        ptrace(PTRACE_CONT if pid == manager else PTRACE_SYSCALL, pid, 0, delivered)
    raise TimeoutError('host did not reach/reset the deterministic resize gate within 180 seconds')


def wait():
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        for name in ('error.json', 'completed.json', 'gate.json'):
            path = STATE / name
            if path.exists():
                result = json.loads(path.read_text())
                assert name == 'gate.json', result
                return result
        time.sleep(0.05)
    raise TimeoutError('resize worker did not reach its deterministic syscall boundary')
