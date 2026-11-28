"""The disposable fixture's private keyboard. Never installed in the OS or
the installer.

A credential reaches an installed program's prompt through a virtual keyboard
inside the disposable guest: the host writes it to the guest's UART after raw
mode disables echo, and it is typed on the real kernel VT. Nothing prints it.
The host runs this text with `__name__` set to something other than
`__main__`, then calls one of the functions below.
"""
import contextlib
import ctypes
import fcntl
import os
import pathlib
import re
import resource
import select
import struct
import subprocess
import termios
import time
import tty

PRIVATE = pathlib.Path('/run/ilex-encrypted-test')
DRIVE_STEP = 'not-started'
KEY_CODES = dict(zip(b'abcdefghijklmnopqrstuvwxyz',
                     [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
                      49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44]))
KEY_CODES[ord('-')] = 12
KEY_CODES[ord('\n')] = 28


def harden():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE


def open_keyboard():
    global DRIVE_STEP
    DRIVE_STEP = 'uinput-load'
    subprocess.run(['modprobe', 'uinput'], check=True)
    DRIVE_STEP = 'uinput-open'
    keyboard = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
    DRIVE_STEP = 'uinput-create'
    try:
        # Linux uinput ABI. This device exists only in this disposable guest.
        fcntl.ioctl(keyboard, 0x40045564, 1)  # UI_SET_EVBIT(EV_KEY)
        for code in set(KEY_CODES.values()):
            fcntl.ioctl(keyboard, 0x40045565, code)
        os.write(keyboard, struct.pack('80sHHHHI', b'ilex-private-test-keyboard', 3, 1, 1, 1, 0) + bytes(1024))
        fcntl.ioctl(keyboard, 0x5501)  # UI_DEV_CREATE
        return keyboard
    except Exception:
        os.close(keyboard)
        raise


def type_private(keyboard, value):
    for code in [KEY_CODES[character] for character in value] + [KEY_CODES[ord('\n')]]:
        for down in (1, 0):
            os.write(keyboard, struct.pack('llHHi', 0, 0, 1, code, down))
            os.write(keyboard, struct.pack('llHHi', 0, 0, 0, 0, 0))
            time.sleep(0.03)


@contextlib.contextmanager
def private_uart(length):
    # The host writes to the UART only after raw mode disables echo. The
    # output-only transcript must never receive credential bytes.
    assert length in (48, 71)
    data = bytearray()
    try:
        with open('/dev/ttyS0', 'rb', buffering=0) as serial:
            settings = termios.tcgetattr(serial)
            try:
                tty.setraw(serial)
                (PRIVATE / 'vt-input-ready').touch()
                deadline = time.monotonic() + 30
                while len(data) < length + 1:
                    assert select.select([serial], [], [], max(0, deadline - time.monotonic()))[0]
                    chunk = serial.read(length + 1 - len(data))
                    assert chunk
                    data.extend(chunk)
                assert data[-1] == ord('\n') and all(char in KEY_CODES and char != ord('\n')
                                                     for char in memoryview(data)[:-1])
            finally:
                termios.tcsetattr(serial, termios.TCSANOW, settings)
        yield memoryview(data)[:-1]
    finally:
        data[:] = bytes(len(data))


def vt_input(length):
    # Input reaches the installed program through the real kernel VT.
    assert pathlib.Path('/sys/class/tty/tty0/active').read_text().strip() == 'tty8'
    keyboard = open_keyboard()
    try:
        with private_uart(length) as data:
            type_private(keyboard, data)
    finally:
        fcntl.ioctl(keyboard, 0x5502)
        os.close(keyboard)


def verify_fallback_key(length, uuid, slot):
    assert re.fullmatch(r'[a-fA-F0-9-]{36}', uuid) and 0 <= slot < 32
    with private_uart(length) as data:
        subprocess.run([
            '/usr/sbin/cryptsetup', 'open', '--test-passphrase', '--type', 'luks2',
            '--disable-external-tokens', '--token-type=none', '--key-file', '-',
            '--key-slot', str(slot), '/dev/disk/by-uuid/' + uuid,
        ], input=data, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
           check=True, timeout=45)
