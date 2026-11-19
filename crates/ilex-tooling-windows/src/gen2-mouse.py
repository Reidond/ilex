"""Read the Linux Hyper-V input device without grabbing it from the session."""
import os
from pathlib import Path
import select
import struct
import time

devices = []
for node in Path("/sys/class/input").glob("event*"):
    name = (node / "device/name").read_text().strip()
    if "Microsoft Vmbus HID" in name:
        devices.append((node.name, name))
assert len(devices) == 1, devices
node, name = devices[0]
event = struct.Struct("@llHHi")
with open("/dev/input/" + node, "rb", buffering=0) as stream:
    print("ready", flush=True)
    deadline = time.monotonic() + 20
    seen = {}
    while time.monotonic() < deadline:
        if select.select([stream], [], [], max(0, deadline - time.monotonic()))[0]:
            _, _, kind, code, value = event.unpack(os.read(stream.fileno(), event.size))
            seen.setdefault((kind, code), set()).add(value)
        if (3, 0) in seen and (3, 1) in seen and {0, 1} <= seen.get((1, 272), set()):
            print(name, node, sorted((kind, code, sorted(values)) for (kind, code), values in seen.items()))
            break
    else:
        raise SystemExit(f"mouse events missing: {seen}")
