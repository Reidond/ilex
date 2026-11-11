//! The whole keyboard, while the viewer has focus, through a low-level
//! keyboard hook: what a hook swallows never reaches Windows, so the Windows
//! key doesn't open Start and Alt+Tab doesn't switch apps; they go to the
//! guest instead, like in any VM console. (Ctrl+Alt+Del stays with Windows:
//! no hook can see it.)
//!
//! Keys travel as QEMU qnums, which are PC set-1 scancodes with `0x80` for
//! the `E0` (extended) keys: exactly Windows' `scanCode` plus
//! `LLKHF_EXTENDED`, so there is no keymap. The guest's layout decides what
//! a key means, as on real hardware.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use ilex_devkit::viewer::ToBridge;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    SetWindowsHookExW, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// The viewer's window; keys are only taken while it is in front.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static SINK: OnceLock<Mutex<Sender<ToBridge>>> = OnceLock::new();
/// Keys the guest has seen go down and not up, to release on focus loss.
static HELD: Mutex<Option<HashSet<u16>>> = Mutex::new(None);

/// Install the hook for `hwnd` (on the thread that runs the message loop).
pub fn install(hwnd: usize, sink: Sender<ToBridge>) -> anyhow::Result<()> {
    WINDOW.store(hwnd, Ordering::Relaxed);
    let _ = SINK.set(Mutex::new(sink));
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if hook.is_null() {
        anyhow::bail!("SetWindowsHookExW(WH_KEYBOARD_LL) failed");
    }
    Ok(())
}

/// The window lost focus: let go of every key the guest thinks is down.
pub fn release_all() {
    let keys: Vec<u16> = HELD
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .drain()
        .collect();
    if let Some(sink) = SINK.get() {
        let sink = sink.lock().unwrap();
        for qnum in keys {
            let _ = sink.send(ToBridge::Key { qnum, down: false });
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ours = WINDOW.load(Ordering::Relaxed);
    if code == HC_ACTION as i32 && ours != 0 && unsafe { GetForegroundWindow() } as usize == ours {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let down = wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize;
        let scan = kb.scanCode & 0x7f;
        if scan != 0 {
            let qnum = (scan
                | if kb.flags & LLKHF_EXTENDED != 0 {
                    0x80
                } else {
                    0
                }) as u16;
            {
                let mut held = HELD.lock().unwrap();
                let held = held.get_or_insert_with(HashSet::new);
                if down {
                    held.insert(qnum);
                } else {
                    held.remove(&qnum);
                }
            }
            if let Some(sink) = SINK.get() {
                let _ = sink.lock().unwrap().send(ToBridge::Key { qnum, down });
            }
            // Swallowed: Windows never sees it.
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
