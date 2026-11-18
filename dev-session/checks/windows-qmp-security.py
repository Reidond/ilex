"""Real QEMU sockets: normal client, suspended child lowered to low IL, normal client.
No users, logons, privileges or system-wide settings are created/changed.
"""
import argparse, ctypes as C, json, subprocess, time
from ctypes import wintypes as W
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--ready', type=Path, required=True)
p.add_argument('--client', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
args = p.parse_args()
info = json.loads(args.ready.read_text())
k = C.WinDLL('kernel32', use_last_error=True)
a = C.WinDLL('advapi32', use_last_error=True)
k.GetCurrentProcess.restype = W.HANDLE
k.CloseHandle.argtypes = [W.HANDLE]
a.OpenProcessToken.argtypes = [W.HANDLE, W.DWORD, C.POINTER(W.HANDLE)]
a.GetTokenInformation.argtypes = [W.HANDLE, C.c_int, C.c_void_p, W.DWORD, C.POINTER(W.DWORD)]
a.SetTokenInformation.argtypes = [W.HANDLE, C.c_int, C.c_void_p, W.DWORD]
a.ConvertStringSidToSidW.argtypes = [W.LPCWSTR, C.POINTER(C.c_void_p)]
a.GetLengthSid.argtypes = [C.c_void_p]
k.LocalFree.argtypes = [C.c_void_p]
k.OpenThread.argtypes = [W.DWORD, W.BOOL, W.DWORD]
k.OpenThread.restype = W.HANDLE
k.ResumeThread.argtypes = [W.HANDLE]
k.CreateToolhelp32Snapshot.restype = W.HANDLE

class Thread(C.Structure):
    _fields_ = [('size', W.DWORD), ('usage', W.DWORD), ('tid', W.DWORD), ('pid', W.DWORD), ('pri', W.LONG), ('base', W.LONG), ('flags', W.DWORD)]

class Label(C.Structure):
    _fields_ = [('sid', C.c_void_p), ('attributes', W.DWORD)]

k.Thread32First.argtypes = [W.HANDLE, C.POINTER(Thread)]
k.Thread32Next.argtypes = [W.HANDLE, C.POINTER(Thread)]

def token_id(handle):
    data = C.create_string_buffer(256)
    length = W.DWORD()
    assert a.GetTokenInformation(handle, 10, data, len(data), C.byref(length))
    return data.raw[:8].hex()

parent = W.HANDLE()
assert a.OpenProcessToken(k.GetCurrentProcess(), 8, C.byref(parent))
parent_id = token_id(parent)
k.CloseHandle(parent)
results = []
try:
    for role in ['control', 'input']:
        endpoint = info[role]['endpoint']
        normal = subprocess.run([str(args.client), '--connect', endpoint], capture_output=True, timeout=10)
        assert normal.returncode == 0, normal.stderr.decode(errors='replace')
        child = subprocess.Popen([str(args.client), '--connect', endpoint], stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, creationflags=4 | 0x08000000)
        token = W.HANDLE()
        sid = C.c_void_p()
        thread = None
        try:
            assert a.OpenProcessToken(int(child._handle), 0x88, C.byref(token))
            child_id = token_id(token)
            assert child_id != parent_id, 'Refusing to alter a token shared with the parent'
            assert a.ConvertStringSidToSidW('S-1-16-4096', C.byref(sid))
            size = C.sizeof(Label) + a.GetLengthSid(sid)
            data = C.create_string_buffer(size)
            label = C.cast(data, C.POINTER(Label))
            label.contents.sid, label.contents.attributes = sid.value, 0x20
            assert a.SetTokenInformation(token, 25, data, size), C.get_last_error()
            snapshot = k.CreateToolhelp32Snapshot(4, 0)
            entry = Thread(); entry.size = C.sizeof(entry)
            tids = []
            try:
                ok = k.Thread32First(snapshot, C.byref(entry))
                while ok:
                    if entry.pid == child.pid:
                        tids.append(entry.tid)
                    ok = k.Thread32Next(snapshot, C.byref(entry))
            finally:
                k.CloseHandle(snapshot)
            assert len(tids) == 1
            thread = k.OpenThread(2, False, tids[0])
            assert thread and k.ResumeThread(thread) == 1
            stdout, stderr = child.communicate(timeout=10)
            error = stderr.decode(errors='replace')
            assert child.returncode != 0 and ('10013' in error or 'os error 5' in error), error
            again = subprocess.run([str(args.client), '--connect', endpoint], capture_output=True, timeout=10)
            assert again.returncode == 0, again.stderr.decode(errors='replace')
            results.append({'role': role, 'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                            'acl': info[role], 'normal_before': True, 'low_integrity_sid': 'S-1-16-4096',
                            'parent_token_id': parent_id, 'child_token_id': child_id,
                            'low_client_exit': child.returncode, 'low_client_error': error,
                            'normal_after': True})
        finally:
            if child.poll() is None:
                child.kill(); child.wait()
            if thread: k.CloseHandle(thread)
            if token: k.CloseHandle(token)
            if sid: k.LocalFree(sid)
    args.out.write_text(json.dumps({'schema': 1, 'results': results}, indent=2), encoding='utf-8')
    print('Both ACL-protected QMP endpoints reject low-integrity clients and accept normal reconnects')
finally:
    args.ready.with_name('transport.stop').touch()
