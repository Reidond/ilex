#!/usr/bin/python3
"""Record delivered guest key events; never grabs the device or injects keys."""
import glob
import json
import os
import select
import struct
import sys
import time

fds = {os.open(path, os.O_RDONLY | os.O_NONBLOCK): path for path in glob.glob('/dev/input/event*')}
until = time.monotonic() + float(sys.argv[1] if len(sys.argv) > 1 else 90)
print('{"ready":true}', flush=True)
while time.monotonic() < until:
    for fd in select.select(list(fds), [], [], .2)[0]:
        data = os.read(fd, 24 * 64)
        for offset in range(0, len(data), 24):
            seconds, micros, kind, code, value = struct.unpack('llHHi', data[offset:offset + 24])
            if kind == 1:
                print(json.dumps({'time': seconds + micros / 1e6, 'device': fds[fd], 'code': code, 'value': value}), flush=True)
