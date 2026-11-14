//! The Windows side of clipboard sharing: plain text (`CF_UNICODETEXT`).
//!
//! Windows' clipboard sequence number changes on every new clipboard
//! content; a thread polls it and sends new text to the guest. Text from the
//! guest is set here and its sequence number remembered, so it doesn't echo
//! back.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use ilex_devkit::viewer::ToBridge;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

/// The sequence number of the clipboard content the guest put there.
static FROM_GUEST: AtomicU32 = AtomicU32::new(0);

/// Send every new Windows clipboard text to the guest (including what is on
/// the clipboard when the viewer starts).
pub fn watch(sink: Sender<ToBridge>) {
    thread::spawn(move || {
        let mut seen = 0;
        loop {
            let seq = unsafe { GetClipboardSequenceNumber() };
            if seq != seen {
                seen = seq;
                if seq != FROM_GUEST.load(Ordering::Relaxed)
                    && let Some(text) = get_text()
                    && sink.send(ToBridge::Clipboard { text }).is_err()
                {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Put the guest's text on the Windows clipboard (its LF line ends as CRLF).
pub fn set_from_guest(text: &str) {
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    if set_text(&text) {
        FROM_GUEST.store(unsafe { GetClipboardSequenceNumber() }, Ordering::Relaxed);
    }
}

/// Another program may hold the clipboard for a moment.
fn open() -> bool {
    for _ in 0..20 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn get_text() -> Option<String> {
    if !open() {
        return None;
    }
    let text = unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT as u32);
        if handle.is_null() {
            None
        } else {
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                None
            } else {
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                GlobalUnlock(handle);
                Some(text)
            }
        }
    };
    unsafe { CloseClipboard() };
    text
}

fn set_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    if !open() {
        return false;
    }
    let ok = unsafe {
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if mem.is_null() {
            false
        } else {
            let ptr = GlobalLock(mem) as *mut u16;
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(mem);
            // The clipboard owns `mem` now.
            !SetClipboardData(CF_UNICODETEXT as u32, mem).is_null()
        }
    };
    unsafe { CloseClipboard() };
    ok
}
