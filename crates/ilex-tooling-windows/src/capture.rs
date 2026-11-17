//! Exact owned HWND capture. Each acquisition uses a new WGC pool, and only a
//! post-request compositor timestamp is accepted. Never restore/activate a VM.
use crate::qemu::VmInfo;
use anyhow::{Context, Result, bail, ensure};
use std::{
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::ClientToScreen,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Dxgi::IDXGIDevice,
        },
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
                RO_INIT_MULTITHREADED, RoInitialize,
            },
        },
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::{
                EnumWindows, GetClientRect, GetWindowThreadProcessId, IsIconic, IsWindow,
                IsWindowVisible,
            },
        },
    },
    core::{BOOL, Interface, factory},
};

pub fn find_window(pid: u32) -> Option<isize> {
    struct Search {
        pid: u32,
        hwnd: isize,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut owner = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut owner));
        }
        if owner == search.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.hwnd = hwnd.0 as isize;
            return false.into();
        }
        true.into()
    }
    let mut search = Search { pid, hwnd: 0 };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM((&mut search as *mut Search) as isize));
    }
    (search.hwnd != 0).then_some(search.hwnd)
}
pub fn validate_window(info: &VmInfo) -> Result<HWND> {
    let hwnd = HWND(info.hwnd as *mut _);
    let mut pid = 0;
    unsafe {
        ensure!(
            IsWindow(Some(hwnd)).as_bool(),
            "QEMU window no longer exists"
        );
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    ensure!(
        pid == info.pid && info.is_running(),
        "QEMU HWND/PID creation identity changed"
    );
    Ok(hwnd)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub crop_x: u32,
    pub crop_y: u32,
}
pub fn geometry(info: &VmInfo) -> Result<Geometry> {
    let hwnd = validate_window(info)?;
    unsafe {
        ensure!(
            !IsIconic(hwnd).as_bool(),
            "minimized: no fresh QEMU client frame"
        );
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let result = (|| -> Result<Geometry> {
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let mut origin = POINT {
                x: client.left,
                y: client.top,
            };
            ensure!(
                ClientToScreen(hwnd, &mut origin).as_bool(),
                "cannot map client origin"
            );
            let mut bounds = RECT::default();
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut bounds as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )?;
            let width = u32::try_from(client.right - client.left)?;
            let height = u32::try_from(client.bottom - client.top)?;
            ensure!(
                width > 0 && height > 0 && width <= 16384 && height <= 16384,
                "empty or unsupported QEMU client geometry"
            );
            Ok(Geometry {
                width,
                height,
                crop_x: u32::try_from(origin.x - bounds.left)?,
                crop_y: u32::try_from(origin.y - bounds.top)?,
            })
        })();
        SetThreadDpiAwarenessContext(old);
        result
    }
}
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub timestamp_ms: u64,
    pub source_ticks: i64,
    pub source: &'static str,
    pub generation: u64,
}

/// Shared by the window observer and every vmctl process, so resizing away
/// and back cannot make an earlier MCP image's coordinates look current.
pub fn record_geometry(info: &VmInfo, size: Geometry) -> Result<u64> {
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(info.dir.join("frame-geometry.lock"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs2::FileExt::try_lock_exclusive(&guard) {
            Ok(()) => break,
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(33))
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error).context("frame geometry lock deadline expired"),
        }
    }
    let path = info.dir.join("frame-geometry.json");
    let previous: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null);
    let key = serde_json::json!([size.width, size.height, size.crop_x, size.crop_y]);
    if !previous.is_null() {
        ilex_tooling_core::check_version(
            previous["schema"]
                .as_u64()
                .context("frame geometry schema missing")?
                .try_into()?,
        )?;
    }
    let generation = if previous["run"] == info.run && previous["geometry"] == key {
        previous["generation"]
            .as_u64()
            .context("invalid frame generation")?
    } else {
        previous["generation"]
            .as_u64()
            .unwrap_or(0)
            .checked_add(1)
            .context("frame generation exhausted")?
    };
    crate::host::atomic(
        &path,
        &serde_json::to_vec(
            &serde_json::json!({"schema":1,"run":info.run,"geometry":key,"generation":generation}),
        )?,
    )?;
    Ok(generation)
}
fn qpc_ticks() -> Result<i64> {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        QueryPerformanceCounter(&mut counter)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    Ok(((counter as i128 * 10_000_000) / frequency as i128).try_into()?)
}
pub fn capture(info: &VmInfo) -> Result<Capture> {
    let before = geometry(info)?;
    if std::env::var("ILEX_CAPTURE_BACKEND").as_deref() == Ok("printwindow") {
        return print_window(info, before);
    }
    let started = qpc_ticks()?;
    let result = wgc(info, before, started);
    match result {
        Ok(frame) => Ok(frame),
        Err(error) => {
            // A timeout/minimize/resize must never turn into an unqualified GDI
            // image. PrintWindow can be explicitly qualified when WGC is absent.
            let reason = format!("{error:#}");
            if !reason.contains("no fresh")
                && !reason.contains("resized")
                && !reason.contains("minimized")
            {
                print_window(info, before).with_context(|| format!("WGC unavailable: {error:#}"))
            } else {
                Err(error)
            }
        }
    }
}
fn wgc(info: &VmInfo, before: Geometry, started: i64) -> Result<Capture> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        ensure!(
            GraphicsCaptureSession::IsSupported()?,
            "Windows Graphics Capture unsupported"
        );
        let hwnd = validate_window(info)?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.context("D3D device missing")?;
        let context = context.context("D3D context missing")?;
        let dxgi: IDXGIDevice = device.cast()?;
        let direct: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(false)?;
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        let result = (|| -> Result<Capture> {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                ensure!(
                    geometry(info)? == before,
                    "QEMU resized during capture; acquire a new frame"
                );
                if let Ok(frame) = pool.TryGetNextFrame() {
                    let ticks = frame.SystemRelativeTime()?.Duration;
                    if ticks < started {
                        frame.Close()?;
                        continue;
                    }
                    let size = frame.ContentSize()?;
                    ensure!(
                        size.Width > 0 && size.Height > 0,
                        "WGC returned an empty frame"
                    );
                    // A fresh pool is recreated for each acquisition, so resize
                    // cannot reuse an undersized old GPU buffer.
                    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                    let texture: ID3D11Texture2D = access.GetInterface()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut desc);
                    ensure!(
                        before.crop_x + before.width <= size.Width as u32
                            && before.crop_y + before.height <= size.Height as u32,
                        "WGC content does not cover the physical client"
                    );
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.MiscFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    let mut staging = None;
                    device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                    let staging = staging.context("D3D staging texture missing")?;
                    context.CopyResource(&staging, &texture);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                    let mut rgb =
                        Vec::with_capacity(before.width as usize * before.height as usize * 3);
                    for y in before.crop_y..before.crop_y + before.height {
                        let row = std::slice::from_raw_parts(
                            (mapped.pData as *const u8).add(
                                y as usize * mapped.RowPitch as usize + before.crop_x as usize * 4,
                            ),
                            before.width as usize * 4,
                        );
                        for pixel in row.as_chunks::<4>().0 {
                            rgb.extend([pixel[2], pixel[1], pixel[0]]);
                        }
                    }
                    context.Unmap(&staging, 0);
                    frame.Close()?;
                    ensure!(
                        geometry(info)? == before,
                        "QEMU resized during readback; acquire again"
                    );
                    let age = (qpc_ticks()? - ticks).max(0) as u64 / 10_000;
                    ensure!(age < 3000, "WGC frame is stale");
                    return Ok(Capture {
                        width: before.width,
                        height: before.height,
                        rgb,
                        timestamp_ms: unix_ms()?.saturating_sub(age),
                        source_ticks: ticks,
                        source: "wgc",
                        generation: record_geometry(info, before)?,
                    });
                }
                if Instant::now() >= deadline {
                    bail!("no fresh WGC frame within 3s (occluded/minimized/inactive display)");
                }
                thread::sleep(Duration::from_millis(10));
            }
        })();
        let _ = session.Close();
        let _ = pool.Close();
        result
    }
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

/// PrintWindow is synchronous and can hang inside a driver. Run it on a bounded
/// worker with at most one in flight per process. It validates the exact
/// non-minimized client, deadline, dimensions and nonblack GL pixels; its
/// source is labelled separately from WGC's compositor timestamp.
fn print_window(info: &VmInfo, before: Geometry) -> Result<Capture> {
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    ensure!(
        !IN_FLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst),
        "PrintWindow worker is still busy; no fresh frame"
    );
    let info = info.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(print_window_inner(&info, before));
        IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    });
    receiver
        .recv_timeout(Duration::from_secs(2))
        .context("PrintWindow deadline expired; no fresh frame")?
}
fn print_window_inner(info: &VmInfo, before: Geometry) -> Result<Capture> {
    use windows::Win32::{
        Graphics::Gdi::*,
        Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
    };
    unsafe {
        struct Dpi(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
        impl Drop for Dpi {
            fn drop(&mut self) {
                unsafe {
                    SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
        let _dpi = Dpi(SetThreadDpiAwarenessContext(
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        ));
        let hwnd = validate_window(info)?;
        ensure!(
            geometry(info)? == before,
            "minimized/resized before PrintWindow"
        );
        let dc = GetDC(Some(hwnd));
        ensure!(!dc.is_invalid(), "QEMU DC unavailable");
        let memory = CreateCompatibleDC(Some(dc));
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: before.width as i32,
                biHeight: -(before.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(memory, bitmap.into());
        let started = Instant::now();
        let success = PrintWindow(hwnd, memory, PRINT_WINDOW_FLAGS(3)).as_bool();
        let result = (|| -> Result<Capture> {
            ensure!(
                success && started.elapsed() < Duration::from_secs(2) && geometry(info)? == before,
                "PrintWindow failed/minimized/resized; no fresh frame"
            );
            let pixels = std::slice::from_raw_parts(
                bits as *const u8,
                before.width as usize * before.height as usize * 4,
            );
            ensure!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[..3].iter().any(|&c| c != 0)),
                "PrintWindow returned black/unqualified GL pixels"
            );
            Ok(Capture {
                width: before.width,
                height: before.height,
                rgb: pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[2], p[1], p[0]])
                    .collect(),
                timestamp_ms: unix_ms()?,
                source_ticks: qpc_ticks()?,
                source: "printwindow-qualified",
                generation: record_geometry(info, before)?,
            })
        })();
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(Some(hwnd), dc);
        result
    }
}
