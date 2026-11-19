"""Observe only the requested Hyper-V input events while the Linux guest runs."""
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import time

kind = sys.argv[1]
wanted = {}
devices = []
for node in Path("/sys/class/input").glob("event*"):
    name = (node / "device/name").read_text().strip()
    if kind == "pointer" and "Microsoft Vmbus HID" in name:
        devices.append(node.name)
    if kind == "keyboard":
        # The firmware keyboard reaches Linux on the Hyper-V serio bus.
        chain = (node / "device").resolve().parts
        if any(part.startswith("serio") for part in chain) and "keyboard" in name.lower():
            devices.append(node.name)
assert len(devices) == 1, "expected one matching input device"
if kind == "pointer":
    x, y = map(int, sys.argv[2:])
    wanted = {(3, 0): x, (3, 1): y}
    button = 272
else:
    assert kind == "keyboard"
    subprocess.run(["systemctl", "is-active", "--quiet", "display-manager.service"], check=True)
    button = 88  # KEY_F12
record = struct.Struct("@llHHi")
boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
with open("/dev/input/" + devices[0], "rb", buffering=0) as stream:
    print("ready", flush=True)
    deadline = time.monotonic() + 20
    axes = {}
    key = set()
    while time.monotonic() < deadline:
        if select.select([stream], [], [], max(0, deadline - time.monotonic()))[0]:
            _, _, event, code, value = record.unpack(os.read(stream.fileno(), record.size))
            if (event, code) in wanted and abs(value - wanted[event, code]) <= 2:
                axes[event, code] = value
            if (event, code) == (1, button):
                key.add(value)
        if axes.keys() == wanted.keys() and {0, 1} <= key:
            print("boot", boot)
            print(kind, devices[0], "target", list(wanted.values()), "observed", list(axes.values()), "press/release")
            break
    else:
        raise SystemExit("expected input was not delivered at the requested coordinates")
