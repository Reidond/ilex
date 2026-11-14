//! The whole keyboard, while the viewer has focus, through a low-level
//! keyboard hook: what a hook swallows never reaches Windows, so the Windows
//! key doesn't open Start and Alt+Tab doesn't switch apps; they go to the
//! guest instead, like in any VM console. (Ctrl+Alt+Del stays with Windows:
//! no hook can see it.)
//!
//! The hook lives in a helper process, a second `ilex-viewer.exe
//! --keyboard-hook <window>`, not in the viewer: Windows was seen not to
//! call a `WH_KEYBOARD_LL` hook of the process that owns the foreground
//! window (it only got calls while some other process had a hook too),
//! while a hook in any other process is called. The helper swallows each
//! key while the viewer is in front and writes it to its stdout, one line
//! per key, which the viewer reads. Its message pump does nothing else, so
//! it also never runs into `LowLevelHooksTimeout`, after which Windows drops
//! a hook for good.
//!
//! Keys the hook didn't take (the helper is gone, or Windows passed the key
//! anyway) reach the window as ordinary key events, and [`from_window`]
//! forwards those by their scan code; nothing is sent twice.
//!
//! Keys travel as QEMU qnums, which are PC set-1 scancodes with `0x80` for
//! the `E0` (extended) keys: exactly Windows' `scanCode` plus
//! `LLKHF_EXTENDED`, so there is no keymap. The guest's layout decides what
//! a key means, as on real hardware.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use ilex_devkit::viewer::ToBridge;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, HC_ACTION, IsWindow, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, MSG, PM_REMOVE, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

static SINK: OnceLock<Mutex<Sender<ToBridge>>> = OnceLock::new();
/// Keys the guest has seen go down and not up, to release on focus loss.
static HELD: Mutex<Option<HashSet<u16>>> = Mutex::new(None);
/// The hook process, killed when the viewer exits.
static HELPER: Mutex<Option<Child>> = Mutex::new(None);
/// `ILEX_VIEWER_STATS`: log what the keyboard does.
static DEBUG: AtomicBool = AtomicBool::new(false);

// ----- in the viewer --------------------------------------------------------

/// Start the hook process for the viewer window `hwnd` and forward what it
/// reports.
pub fn install(hwnd: usize, sink: Sender<ToBridge>) -> anyhow::Result<()> {
    let _ = SINK.set(Mutex::new(sink));
    DEBUG.store(
        std::env::var_os("ILEX_VIEWER_STATS").is_some(),
        Ordering::Relaxed,
    );
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--keyboard-hook", &hwnd.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    *HELPER.lock().unwrap() = Some(child);
    thread::Builder::new()
        .name("keyboard-hook".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let mut parts = line.split_whitespace();
                if let (Some(q), Some(d)) = (parts.next(), parts.next())
                    && let Ok(qnum) = q.parse::<u16>()
                {
                    if DEBUG.load(Ordering::Relaxed) {
                        eprintln!("ilex-viewer: hook: key {qnum:#04x} {d}");
                    }
                    send(qnum, d == "down");
                }
            }
            eprintln!(
                "ilex-viewer: the keyboard hook process ended; the Windows key goes to Windows now"
            );
        })?;
    Ok(())
}

/// Stop the hook process (the viewer is closing).
pub fn shutdown() {
    if let Some(mut child) = HELPER.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A key reached the window as an ordinary key event (winit's scan code:
/// set 1, `0xE0..` for extended keys), so the hook didn't take it: forward
/// it.
pub fn from_window(scancode: u32, down: bool) {
    let qnum = (scancode & 0x7f) | if scancode & 0xff00 == 0xe000 { 0x80 } else { 0 };
    if qnum & 0x7f == 0 {
        return;
    }
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("ilex-viewer: key {qnum:#04x} came past the hook");
    }
    send(qnum as u16, down);
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

fn send(qnum: u16, down: bool) {
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
}

// ----- in the hook process --------------------------------------------------

/// The viewer window this hook process works for.
static WINDOW: AtomicUsize = AtomicUsize::new(0);

/// `ilex-viewer --keyboard-hook <hwnd>`: hook the keyboard until the window
/// is gone.
pub fn hook_process(hwnd: usize) {
    WINDOW.store(hwnd, Ordering::Relaxed);
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if hook.is_null() {
        eprintln!("ilex-viewer: SetWindowsHookExW(WH_KEYBOARD_LL) failed");
        return;
    }
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // Pumping messages is what lets Windows call the hook on this thread.
    while unsafe { IsWindow(hwnd as _) } != 0 {
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ours = WINDOW.load(Ordering::Relaxed);
    if code == HC_ACTION as i32 && unsafe { GetForegroundWindow() } as usize == ours {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let down = wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize;
        let scan = kb.scanCode & 0x7f;
        // Unicode text injected with KEYEVENTF_UNICODE (VK_PACKET) has no
        // real scancode: let it through to the window.
        if scan != 0 && kb.vkCode != 0xe7 {
            let qnum = scan
                | if kb.flags & LLKHF_EXTENDED != 0 {
                    0x80
                } else {
                    0
                };
            let mut out = std::io::stdout().lock();
            let sent = writeln!(out, "{qnum} {}", if down { "down" } else { "up" })
                .and_then(|_| out.flush());
            if sent.is_ok() {
                // Swallowed: Windows never sees it.
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
