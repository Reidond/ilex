import argparse
import ctypes as C
from ctypes import wintypes as W
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--info', type=Path, required=True)
p.add_argument('--vmctl', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
a = p.parse_args()
i = json.loads(a.info.read_text())
assert i['owner'] == '64addbb8-4a8c-8d86-b005-4b17d6877a97'
a.out.mkdir(parents=True, exist_ok=True)
u = C.WinDLL('user32', use_last_error=True)
u.GetForegroundWindow.restype = W.HWND
u.SetThreadDpiAwarenessContext.argtypes = [C.c_void_p]
u.SetThreadDpiAwarenessContext.restype = C.c_void_p
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.GetWindowRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.IsIconic.argtypes = [W.HWND]
u.SetCursorPos.argtypes = [C.c_int, C.c_int]
pid = W.DWORD()
assert u.GetWindowThreadProcessId(i['hwnd'], C.byref(pid)) and pid.value == i['pid']
assert u.GetForegroundWindow() == i['hwnd'] and not u.IsIconic(i['hwnd'])
assert not any(u.GetAsyncKeyState(v) & 0x8000 for v in [0xa0,0xa1,0xa2,0xa3,0xa4,0xa5,0x5b,0x5c])
previous = u.SetThreadDpiAwarenessContext(C.c_void_p(-4))
assert previous
try:
    rect, client = W.RECT(), W.RECT()
    assert u.GetWindowRect(i['hwnd'], C.byref(rect)) and u.GetClientRect(i['hwnd'], C.byref(client))
    assert [client.right, client.bottom] == [1496, 903]
    # Minimize caption button grounded on the fresh 1500x950 Cua screenshot.
    point = [rect.left + 1322, rect.top + 21]
    assert u.SetCursorPos(*point)
finally:
    u.SetThreadDpiAwarenessContext(previous)
class Mouse(C.Structure):
    _fields_ = [('x', W.LONG), ('y', W.LONG), ('data', W.DWORD), ('flags', W.DWORD), ('time', W.DWORD), ('extra', C.c_size_t)]
class Union(C.Union):
    _fields_ = [('mouse', Mouse), ('padding', C.c_byte * 32)]
class Input(C.Structure):
    _fields_ = [('kind', W.DWORD), ('data', Union)]
u.SendInput.argtypes = [W.UINT, C.POINTER(Input), C.c_int]
result = {'schema': 1, 'run': i['run'], 'started_utc': datetime.now(timezone.utc).isoformat(),
          'method': 'real SendInput mouse down/up at Cua-grounded minimize caption', 'screen_point': point}
for flags in [2, 4]:
    event = Input()
    event.kind = 0
    event.data.mouse.flags = flags
    assert u.SendInput(1, C.byref(event), C.sizeof(event)) == 1
    time.sleep(.05)
until = time.monotonic() + 3
while not u.IsIconic(i['hwnd']) and time.monotonic() < until:
    time.sleep(.05)
result['is_iconic_before_capture'] = bool(u.IsIconic(i['hwnd']))
assert result['is_iconic_before_capture']
png = a.out / 'minimized-direct.png'
assert not png.exists()
captured = subprocess.run([str(a.vmctl), '--vm', i['purpose'], 'screenshot', '-o', str(png)], capture_output=True, timeout=15)
result.update({'finished_utc': datetime.now(timezone.utc).isoformat(), 'capture_exit': captured.returncode,
               'is_iconic_after_capture': bool(u.IsIconic(i['hwnd'])), 'png_created': png.exists()})
(a.out / 'minimized-direct.stdout.log').write_bytes(captured.stdout)
(a.out / 'minimized-direct.stderr.log').write_bytes(captured.stderr)
(a.out / 'minimized-direct-result.json').write_text(json.dumps(result, indent=2))
print(json.dumps(result))
assert result['is_iconic_after_capture'] and captured.returncode != 0 and not png.exists()
assert b'no fresh' in captured.stderr
