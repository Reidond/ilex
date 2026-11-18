//! Native Windows clipboard notifications over the pinned development SSH.
//! Only an unlocked guest session's current generation accepts text. Bounded
//! queues/UTF-8 frames, explicit origins/sequences, no content or errors logged.
use crate::{host, qemu::VmInfo};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND},
        System::{DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

const MAX_TEXT: usize = 256 * 1024;
const MAX_FRAME: usize = 2 * 1024 * 1024;
const HELPER: &str = "/run/ilex-dev-session/adapter.py";

/// Refresh only this active VM's public session code. Private host identity
/// stays in a different 0700 directory. No runtime/package setup by the owner.
pub fn prepare(info: &VmInfo) -> Result<()> {
    info.validate()?;
    let source = include_bytes!("../../../dev-session/session.py");
    let (job, mut child) = host::spawn_owned(info.ssh().arg("install -d -m 0755 -o root -g root /run/ilex-dev-session && umask 022 && cat > /run/ilex-dev-session/adapter.py && chmod 0755 /run/ilex-dev-session/adapter.py").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()))?;
    let mut input = child.stdin.take().context("session upload stdin missing")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let writer = thread::spawn(move || {
        let result = input.write_all(source);
        drop(input);
        let _ = sender.send(result);
    });
    let result = (|| -> Result<()> {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .context("session upload deadline expired")??;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "session upload failed");
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "session upload acknowledgement timed out"
            );
            thread::sleep(Duration::from_millis(20));
        }
        host::atomic(
            &info.dir.join("session-helper.json"),
            &serde_json::to_vec(
                &json!({"schema":1,"sha256":host::hash(source),"guest_path":HELPER}),
            )?,
        )
    })();
    let _ = child.kill();
    let _ = child.wait();
    drop(job);
    let _ = writer.join();
    result
}
pub struct Session {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

/// SDL's physical client resize drives virtio EDID. Observe the guest's actual
/// mode and, on Hyprland, apply an advertised mode via its runtime API.
pub struct ResizeSession {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl ResizeSession {
    pub fn start(info: VmInfo) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut previous = None;
            let mut changed = Instant::now();
            let mut next_probe = Instant::now();
            let mut attempts = 0u64;
            let mut session = Value::Null;
            let mut output = Value::Null;
            let mut session_changes = 0u64;
            let mut output_changes = 0u64;
            while !stopped.load(Ordering::Relaxed) && info.is_running() {
                if let Ok(geometry) = crate::capture::geometry(&info) {
                    let _ = crate::capture::record_geometry(&info, geometry);
                    let size = (geometry.width, geometry.height);
                    if previous != Some(size) {
                        previous = Some(size);
                        changed = Instant::now();
                        next_probe = changed + Duration::from_millis(300);
                    }
                    if changed.elapsed() > Duration::from_millis(300)
                        && Instant::now() >= next_probe
                    {
                        attempts = attempts.saturating_add(1);
                        // Every attempt has its own five-second deadline. Keep
                        // probing after success too: logind/compositor/output
                        // changes do not necessarily resize the host window.
                        let mut value = display_info(&info, Some(size)).unwrap_or_else(|_| json!({"schema":1,"acknowledged":false,"state":"guest-display-unavailable","target":[size.0,size.1]}));
                        if value["session"] != session {
                            session_changes = session_changes.saturating_add(1);
                            session = value["session"].clone();
                        }
                        let observed_output = json!({"backend":value["backend"],"edid":value["edid"],"monitors":value["monitors"],"display_instance":value["display_instance"]});
                        if observed_output != output {
                            output_changes = output_changes.saturating_add(1);
                            output = observed_output;
                        }
                        if crate::capture::geometry(&info).ok() != Some(geometry) {
                            value["acknowledged"] = json!(false);
                            value["state"] = json!("host-resized-during-probe");
                        }
                        value["run"] = json!(info.run);
                        value["attempt"] = json!(attempts);
                        value["session_changes"] = json!(session_changes);
                        value["output_changes"] = json!(output_changes);
                        value["observed_unix_ms"] = json!(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis()
                        );
                        let _ = host::atomic(
                            &info.dir.join("resize-status.json"),
                            &serde_json::to_vec(&value).unwrap_or_default(),
                        );
                        next_probe = Instant::now() + Duration::from_secs(2);
                    }
                }
                for _ in 0..5 {
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for ResizeSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn display_info(info: &VmInfo, target: Option<(u32, u32)>) -> Result<Value> {
    info.validate()?;
    let request = target.map_or_else(
        || format!("{HELPER} --display-info"),
        |(w, h)| format!("{HELPER} --resize {w} {h}"),
    );
    let (job, mut child) = host::spawn_owned(
        info.ssh()
            .arg(request)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )?;
    let mut output = child
        .stdout
        .take()
        .context("display probe stdout missing")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .by_ref()
            .take(65537)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = (|| -> Result<Value> {
        let bytes = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("guest mode acknowledgement deadline expired")??;
        ensure!(bytes.len() <= 65536, "display probe output limit");
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "display probe exit deadline expired"
            );
            thread::sleep(Duration::from_millis(20));
        };
        ensure!(status.success(), "guest mode unavailable");
        Ok(serde_json::from_slice(&bytes)?)
    })();
    let _ = child.kill();
    let _ = child.wait();
    drop(job);
    let _ = reader.join();
    result
}
impl Session {
    pub fn start(info: VmInfo) -> Result<Self> {
        info.validate()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) && info.is_running() {
                if bridge(&info, &stopped).is_err() {
                    // A generic lifecycle diagnostic never includes contents.
                    eprintln!(
                        "ilex: dev clipboard channel disconnected; retrying while VM is active"
                    );
                }
                for _ in 0..20 {
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn read_frames(
    mut stream: impl Read,
    sender: SyncSender<Value>,
    pending: Arc<std::sync::Mutex<Option<Instant>>>,
) {
    let result = (|| -> Result<()> {
        loop {
            let mut header = [0; 4];
            stream.read_exact(&mut header[..1])?;
            *pending
                .lock()
                .map_err(|_| anyhow::anyhow!("reader state poisoned"))? = Some(Instant::now());
            stream.read_exact(&mut header[1..])?;
            let size = u32::from_be_bytes(header) as usize;
            ensure!(size > 0 && size <= MAX_FRAME, "clipboard frame limit");
            let mut data = vec![0; size];
            stream.read_exact(&mut data)?;
            let value: Value = serde_json::from_slice(&data).context("invalid clipboard frame")?;
            if let Some(text) = value.get("text") {
                let text = text.as_str().context("invalid clipboard text")?;
                ensure!(
                    text.len() <= MAX_TEXT && !text.contains('\0'),
                    "clipboard text limit"
                );
            }
            // Backpressure terminates this connection rather than retaining text.
            sender
                .try_send(value)
                .context("clipboard receive queue full")?;
            *pending
                .lock()
                .map_err(|_| anyhow::anyhow!("reader state poisoned"))? = None;
        }
    })();
    let _ = result;
}
fn write_frames(
    mut stream: impl Write,
    receiver: Receiver<Value>,
    pending: Arc<std::sync::Mutex<Option<Instant>>>,
) {
    let _ = (|| -> Result<()> {
        for value in receiver {
            *pending
                .lock()
                .map_err(|_| anyhow::anyhow!("writer state poisoned"))? = Some(Instant::now());
            let data = serde_json::to_vec(&value)?;
            ensure!(data.len() <= MAX_FRAME, "clipboard frame limit");
            stream.write_all(&(data.len() as u32).to_be_bytes())?;
            stream.write_all(&data)?;
            stream.flush()?;
            *pending
                .lock()
                .map_err(|_| anyhow::anyhow!("writer state poisoned"))? = None;
        }
        Ok(())
    })();
}
struct Listener(HWND);
impl Listener {
    fn new() -> Result<Self> {
        unsafe extern "system" fn proc(
            hwnd: HWND,
            message: u32,
            w: windows::Win32::Foundation::WPARAM,
            l: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT {
            unsafe { DefWindowProcW(hwnd, message, w, l) }
        }
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: w!("ilex-dev-clipboard"),
                ..Default::default()
            };
            let _ = RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("ilex-dev-clipboard"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )?;
            AddClipboardFormatListener(hwnd)?;
            Ok(Self(hwnd))
        }
    }
    fn changed(&self) -> bool {
        unsafe {
            let mut dirty = false;
            let mut message = MSG::default();
            while PeekMessageW(&mut message, Some(self.0), 0, 0, PM_REMOVE).as_bool() {
                dirty |= message.message == WM_CLIPBOARDUPDATE;
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            dirty
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveClipboardFormatListener(self.0);
            let _ = DestroyWindow(self.0);
        }
    }
}
pub fn read_clipboard() -> Result<Option<String>> {
    unsafe {
        if !IsClipboardFormatAvailable(13).is_ok() {
            return Ok(None);
        }
        OpenClipboard(None)?;
        let result = (|| -> Result<Option<String>> {
            let handle = GetClipboardData(13)?;
            let global = windows::Win32::Foundation::HGLOBAL(handle.0);
            let bytes = GlobalSize(global);
            ensure!(bytes <= 2 * (MAX_TEXT + 1), "clipboard text limit");
            let pointer = GlobalLock(global);
            ensure!(!pointer.is_null(), "clipboard memory unavailable");
            let data = std::slice::from_raw_parts(pointer as *const u16, bytes / 2);
            let result = data
                .iter()
                .position(|&c| c == 0)
                .context("unterminated clipboard text")
                .and_then(|len| Ok(String::from_utf16(&data[..len])?));
            let _ = GlobalUnlock(global);
            let text = result?;
            ensure!(text.len() <= MAX_TEXT, "clipboard text limit");
            Ok(Some(text))
        })();
        let _ = CloseClipboard();
        result
    }
}
pub fn write_clipboard(text: &str, owner: Option<HWND>) -> Result<u32> {
    ensure!(
        text.len() <= MAX_TEXT && !text.contains('\0'),
        "clipboard text limit"
    );
    let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        OpenClipboard(owner)?;
        let result = (|| -> Result<u32> {
            let memory = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2)?;
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(Some(memory));
                anyhow::bail!("clipboard memory unavailable");
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer as *mut u16, data.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) =
                EmptyClipboard().and_then(|()| SetClipboardData(13, Some(HANDLE(memory.0))))
            {
                let _ = windows::Win32::Foundation::GlobalFree(Some(memory));
                return Err(error.into());
            }
            Ok(GetClipboardSequenceNumber())
        })();
        let _ = CloseClipboard();
        result
    }
}
fn bridge(info: &VmInfo, stop: &AtomicBool) -> Result<()> {
    let listener = Listener::new()?;
    let (job, mut child) = host::spawn_owned(
        info.ssh()
            .arg(format!("{HELPER} --supervise"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )?;
    let (received, incoming) = mpsc::sync_channel(8);
    let (outgoing, sending) = mpsc::sync_channel(8);
    let input = child.stdout.take().context("clipboard stdout missing")?;
    let output = child.stdin.take().context("clipboard stdin missing")?;
    let reading = Arc::new(std::sync::Mutex::new(None));
    let writing = Arc::new(std::sync::Mutex::new(None));
    let reader_pending = reading.clone();
    let writer_pending = writing.clone();
    let reader = thread::spawn(move || read_frames(input, received, reader_pending));
    let writer = thread::spawn(move || write_frames(output, sending, writer_pending));
    let mut generation = None;
    let mut host_sequence = 0u64;
    let mut guest_sequence = 0u64;
    let mut own_sequence = 0;
    let mut last_text: Option<String> = None;
    let mut host_updates = 0;
    let mut guest_updates = 0;
    let mut suppressed = 0;
    let mut activations = 0u64;
    let mut deactivations = 0u64;
    let result = (|| -> Result<()> {
        let mut initial = false;
        let mut last_status = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) || !info.is_running() {
                break;
            }
            ensure!(
                child.try_wait()?.is_none() && !reader.is_finished() && !writer.is_finished(),
                "clipboard transport ended"
            );
            for pending in [&reading, &writing] {
                ensure!(
                    pending
                        .lock()
                        .map_err(|_| anyhow::anyhow!("clipboard IO state poisoned"))?
                        .is_none_or(|started| started.elapsed() < Duration::from_secs(3)),
                    "clipboard IO deadline expired"
                );
            }
            while let Ok(value) = incoming.try_recv() {
                ensure!(value["schema"] == 1, "clipboard schema mismatch");
                match value["op"].as_str() {
                    Some("ready") => {
                        let value = value["generation"]
                            .as_str()
                            .context("clipboard generation missing")?;
                        ensure!(value.len() <= 64, "clipboard generation limit");
                        generation = Some(value.to_owned());
                        host_sequence = 0;
                        guest_sequence = 0;
                        last_text = None;
                        initial = true;
                        activations += 1;
                        eprintln!("ilex: dev clipboard active in an unlocked guest session");
                    }
                    Some("inactive") => {
                        deactivations += u64::from(generation.is_some());
                        generation = None;
                        last_text = None;
                        initial = false;
                        eprintln!("ilex: dev clipboard suspended (no unlocked guest session)");
                    }
                    Some("text")
                        if value["origin"] == "guest"
                            && generation.as_deref() == value["generation"].as_str()
                            && generation.is_some() =>
                    {
                        let sequence = value["sequence"]
                            .as_u64()
                            .context("clipboard sequence missing")?;
                        if sequence <= guest_sequence {
                            suppressed += 1;
                            continue;
                        }
                        guest_sequence = sequence;
                        let text = value["text"].as_str().context("clipboard text missing")?;
                        if last_text.as_deref() == Some(text) {
                            suppressed += 1;
                            continue;
                        }
                        own_sequence = write_clipboard(text, Some(listener.0))?;
                        last_text = Some(text.to_owned());
                        guest_updates += 1;
                    }
                    _ => {}
                }
            }
            let changed = listener.changed();
            if let Some(generation) = &generation
                && (initial || changed)
            {
                initial = false;
                let sequence = unsafe { GetClipboardSequenceNumber() };
                if sequence == own_sequence {
                    suppressed += 1;
                } else if let Ok(Some(text)) = read_clipboard() {
                    if last_text.as_deref() == Some(&text) {
                        suppressed += 1;
                    } else {
                        host_sequence = host_sequence
                            .checked_add(1)
                            .context("clipboard sequence exhausted")?;
                        outgoing.try_send(json!({"op":"set","schema":1,"origin":"host","sequence":host_sequence,"generation":generation,"text":text})).context("clipboard send queue full")?;
                        last_text = Some(text);
                        host_updates += 1;
                    }
                }
            }
            if last_status.elapsed() > Duration::from_secs(1) {
                host::atomic(
                    &info.dir.join("session-status.json"),
                    &serde_json::to_vec(
                        &json!({"schema":1,"active":generation.is_some(),"activations":activations,"deactivations":deactivations,"host_updates":host_updates,"guest_updates":guest_updates,"suppressed":suppressed}),
                    )?,
                )?;
                last_status = Instant::now();
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })();
    drop(outgoing);
    let _ = child.kill();
    let _ = child.wait();
    drop(job);
    let _ = reader.join();
    let _ = writer.join();
    let _ = host::atomic(
        &info.dir.join("session-status.json"),
        b"{\"schema\":1,\"active\":false}",
    );
    result
}
