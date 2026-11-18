"""Bounded native Unicode clipboard access for known e2e text; never logs it."""
import ctypes
from ctypes import wintypes as W
import time

user = ctypes.WinDLL('user32', use_last_error=True)
kernel = ctypes.WinDLL('kernel32', use_last_error=True)
user.GetClipboardData.restype = W.HANDLE
user.SetClipboardData.argtypes = [W.UINT, W.HANDLE]
kernel.GlobalAlloc.restype = W.HANDLE
kernel.GlobalLock.argtypes = [W.HANDLE]
kernel.GlobalLock.restype = ctypes.c_void_p
kernel.GlobalUnlock.argtypes = [W.HANDLE]
kernel.GlobalSize.argtypes = [W.HANDLE]
kernel.GlobalSize.restype = ctypes.c_size_t


def open_clipboard():
    for _ in range(50):
        if user.OpenClipboard(None):
            return
        time.sleep(.02)
    raise RuntimeError('clipboard busy')


def read():
    open_clipboard()
    try:
        handle = user.GetClipboardData(13)
        if not handle:
            return None
        assert kernel.GlobalSize(handle) <= 524290
        pointer = kernel.GlobalLock(handle)
        try:
            return ctypes.wstring_at(pointer)
        finally:
            kernel.GlobalUnlock(handle)
    finally:
        user.CloseClipboard()


def write(text, allow_oversize=False):
    # The negative e2e probe exceeds the production limit by one UTF-8 emoji.
    limit = 262148 if allow_oversize else 262144
    assert len(text.encode('utf-8')) <= limit and '\0' not in text
    data = text.encode('utf-16-le') + b'\0\0'
    handle = kernel.GlobalAlloc(2, len(data))
    pointer = kernel.GlobalLock(handle)
    ctypes.memmove(pointer, data, len(data))
    kernel.GlobalUnlock(handle)
    open_clipboard()
    try:
        user.EmptyClipboard()
        assert user.SetClipboardData(13, handle)
    finally:
        user.CloseClipboard()
