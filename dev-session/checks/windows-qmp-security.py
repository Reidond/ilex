"""Real QEMU sockets: raw normal client, same user at low IL, raw normal client.
This qualifies mandatory integrity enforcement, not a different user principal.
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
a.ConvertSidToStringSidW.argtypes = [C.c_void_p, C.POINTER(W.LPWSTR)]
a.GetLengthSid.argtypes = [C.c_void_p]
a.GetSidSubAuthorityCount.argtypes = [C.c_void_p]
a.GetSidSubAuthorityCount.restype = C.POINTER(C.c_ubyte)
a.GetSidSubAuthority.argtypes = [C.c_void_p, W.DWORD]
a.GetSidSubAuthority.restype = C.POINTER(W.DWORD)
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

def token_user(handle):
    data = C.create_string_buffer(4096)
    length = W.DWORD()
    assert a.GetTokenInformation(handle, 1, data, len(data), C.byref(length))
    user_sid = C.cast(data, C.POINTER(Label)).contents.sid
    text = W.LPWSTR()
    assert a.ConvertSidToStringSidW(user_sid, C.byref(text))
    try:
        return text.value
    finally:
        k.LocalFree(C.cast(text, C.c_void_p))

parent = W.HANDLE()
assert a.OpenProcessToken(k.GetCurrentProcess(), 8, C.byref(parent))
parent_id = token_id(parent)
parent_user = token_user(parent)
k.CloseHandle(parent)
results = []
try:
    for role in ['control', 'input']:
        endpoint = info[role]['endpoint']
        normal = subprocess.run([str(args.client), '--raw-connect', endpoint], capture_output=True, timeout=10)
        assert normal.returncode == 0, normal.stderr.decode(errors='replace')
        before_reply = json.loads(normal.stdout)
        assert before_reply['production_acl_preflight'] is False and before_reply['qmp_greeting']
        child = subprocess.Popen([str(args.client), '--raw-connect', endpoint], stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, creationflags=4 | 0x08000000)
        token = W.HANDLE()
        sid = C.c_void_p()
        thread = None
        try:
            assert a.OpenProcessToken(int(child._handle), 0x88, C.byref(token))
            child_id = token_id(token)
            child_user = token_user(token)
            assert child_user == parent_user, 'Low-integrity probe must retain the same TokenUser'
            assert child_id != parent_id, 'Refusing to alter a token shared with the parent'
            assert a.ConvertStringSidToSidW('S-1-16-4096', C.byref(sid))
            size = C.sizeof(Label) + a.GetLengthSid(sid)
            data = C.create_string_buffer(size)
            label = C.cast(data, C.POINTER(Label))
            label.contents.sid, label.contents.attributes = sid.value, 0x20
            assert a.SetTokenInformation(token, 25, data, size), C.get_last_error()
            verified = C.create_string_buffer(256)
            required = W.DWORD()
            assert a.GetTokenInformation(token, 25, verified, len(verified), C.byref(required))
            integrity = C.cast(verified, C.POINTER(Label)).contents.sid
            count = a.GetSidSubAuthorityCount(integrity)[0]
            integrity_rid = a.GetSidSubAuthority(integrity, count - 1)[0]
            assert integrity_rid == 4096, 'Child token did not become low integrity'
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
            again = subprocess.run([str(args.client), '--raw-connect', endpoint], capture_output=True, timeout=10)
            assert again.returncode == 0, again.stderr.decode(errors='replace')
            after_reply = json.loads(again.stdout)
            assert after_reply['production_acl_preflight'] is False and after_reply['qmp_greeting']
            results.append({'role': role, 'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                            'acl': info[role], 'normal_before': True, 'low_integrity_sid': 'S-1-16-4096',
                            'parent_token_id': parent_id, 'child_token_id': child_id,
                            'parent_user_sid': parent_user, 'child_user_sid': child_user,
                            'different_user_principal': False, 'connector': 'raw-winsock-af-unix',
                            'production_acl_preflight': False,
                            'verified_child_integrity_rid': integrity_rid,
                            'low_client_exit': child.returncode, 'low_client_error': error,
                            'normal_after': True})
            results[-1].update(normal_before_reply=before_reply, normal_after_reply=after_reply)
        finally:
            if child.poll() is None:
                child.kill(); child.wait()
            if thread: k.CloseHandle(thread)
            if token: k.CloseHandle(token)
            if sid: k.LocalFree(sid)
    args.out.write_text(json.dumps({'schema': 1, 'qualification': 'same-user mandatory-integrity check', 'cross_user_live_test': False, 'results': results}, indent=2), encoding='utf-8')
    print('Both ACL-protected QMP endpoints reject low-integrity clients and accept normal reconnects')
finally:
    args.ready.with_name('transport.stop').touch()
