//! Frontend dispatch for QMP and the owned Hyper-V console.
use crate::{Button, Frame, Vmctl, WaitCond, backend};
use anyhow::{Result, bail};
use ilex_devkit::{
    Workspace,
    core::{self, VmControl, VmHandle},
};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub enum Attached {
    Qemu(Box<Vmctl>),
    #[cfg(target_os = "windows")]
    Gen2(Box<ilex_tooling_windows::gen2::Vm>),
}

impl Attached {
    pub fn attach(ws: &Workspace, purpose: Option<&str>) -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            let mut candidates = gen2(ws)?;
            candidates.retain(|v| purpose.is_none_or(|p| v.purpose() == p));
            if !candidates.is_empty() {
                let qemu = backend::running_vms(ws)
                    .into_iter()
                    .filter(|v| purpose.is_none_or(|p| v.purpose == p))
                    .count();
                anyhow::ensure!(
                    qemu + candidates.len() == 1,
                    "several native VMs match; choose --vm"
                );
                return Ok(Self::Gen2(Box::new(candidates.remove(0))));
            }
        }
        Ok(Self::Qemu(Box::new(Vmctl::attach(ws, purpose)?)))
    }

    pub fn list(ws: &Workspace) -> Result<Vec<String>> {
        let mut lines: Vec<_> = backend::running_vms(ws)
            .into_iter()
            .map(|v| {
                format!(
                    "{}\t{}\tpid {}\t{}x{} {}\t{}",
                    v.purpose,
                    v.name,
                    v.pid,
                    v.width,
                    v.height,
                    v.display,
                    v.ssh_command_line()
                )
            })
            .collect();
        #[cfg(target_os = "windows")]
        for vm in gen2(ws)? {
            lines.push(format!(
                "{}\t{}\tUUID {}\tHyper-V Gen2",
                vm.purpose(),
                vm.name(),
                vm.handle().opaque_id
            ));
        }
        #[cfg(not(target_os = "windows"))]
        let _ = &mut lines;
        Ok(lines)
    }

    pub fn name(&self) -> String {
        match self {
            Self::Qemu(vm) => vm.info.name.clone(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.name(),
        }
    }
    pub fn handle(&self) -> VmHandle {
        match self {
            Self::Qemu(vm) => vm.handle(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.handle(),
        }
    }
    pub fn status(&self) -> Result<Value> {
        match self {
            Self::Qemu(vm) => vm.status(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.status(),
        }
    }
    pub fn display(&self) -> Result<Value> {
        #[cfg(target_os = "windows")]
        if let Self::Qemu(vm) = self {
            return ilex_tooling_windows::session::display_info(&vm.info, None);
        }
        bail!("guest compositor mode inspection requires the Windows WHPX dev session")
    }
    pub fn capabilities(&self) -> Result<core::VmCapabilities> {
        match self {
            Self::Qemu(vm) => Ok(vm.capabilities()),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.capabilities(),
        }
    }
    pub fn capture(&self) -> Result<core::Frame> {
        match self {
            Self::Qemu(vm) => vm.capture(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.capture(),
        }
    }
    pub fn view_generation(&self) -> u64 {
        match self {
            Self::Qemu(vm) => vm.view_generation(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.generation(),
        }
    }
    pub fn screenshot_png(&self) -> Result<Vec<u8>> {
        match self {
            Self::Qemu(vm) => vm.screenshot_png(),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => image(vm.capture()?).encode_png(),
        }
    }
    pub fn screenshot(&self) -> Result<Frame> {
        Frame::decode(&self.screenshot_png()?)
    }
    pub fn save_screenshot(&self, path: &Path) -> Result<Frame> {
        match self {
            Self::Qemu(vm) => vm.save_screenshot(path),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => {
                let core = vm.capture()?;
                std::fs::write(
                    path.with_extension("frame.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "schema":1,"vm":core.vm,"captured_unix_ms":core.captured_unix_ms,"generation":core.generation,
                        "width":core.width,"height":core.height,"source":ilex_tooling_windows::gen2::SCREENSHOT_SOURCE
                    }))?,
                )?;
                let frame = image(core);
                std::fs::write(path, frame.encode_png()?)?;
                Ok(frame)
            }
        }
    }
    pub fn wait_screen(&self, cond: &WaitCond, timeout: Duration) -> Result<(Vec<u8>, Frame)> {
        crate::wait_screen(cond, timeout, || self.screenshot_png())
    }
    pub fn click(&self, x: u32, y: u32, button: Button, count: u32) -> Result<()> {
        match self {
            Self::Qemu(vm) => vm.click(x, y, button, count),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => {
                anyhow::ensure!(
                    button == Button::Left && count == 1,
                    "Gen2 supports a single left click"
                );
                let frame = vm.capture()?;
                vm.pointer(&frame, frame.generation, x, y)
            }
        }
    }
    pub fn move_to(&self, x: u32, y: u32) -> Result<()> {
        match self {
            Self::Qemu(vm) => vm.move_to(x, y),
            #[cfg(target_os = "windows")]
            Self::Gen2(_) => bail!("standalone Gen2 pointer motion is unqualified"),
        }
    }
    pub fn scroll(&self, x: u32, y: u32, up: bool, amount: u32) -> Result<()> {
        match self {
            Self::Qemu(vm) => vm.scroll(x, y, up, amount),
            #[cfg(target_os = "windows")]
            Self::Gen2(_) => bail!("Gen2 wheel input is unqualified"),
        }
    }
    pub fn type_text(&self, text: &str) -> Result<()> {
        match self {
            Self::Qemu(vm) => vm.type_text(text),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.key(&core::Input::Text {
                text: text.into(),
                layout: "us".into(),
                sensitive: true,
            }),
        }
    }
    pub fn key(&self, combo: &str) -> Result<()> {
        match self {
            Self::Qemu(vm) => vm.key(combo),
            #[cfg(target_os = "windows")]
            Self::Gen2(vm) => vm.key(&core::Input::Chord {
                codes: crate::keys::parse_combo(combo)?
                    .into_iter()
                    .map(scancode)
                    .collect::<Result<_>>()?,
            }),
        }
    }
}

#[cfg(target_os = "windows")]
fn gen2(ws: &Workspace) -> Result<Vec<ilex_tooling_windows::gen2::Vm>> {
    if ws.root.as_os_str().is_empty() {
        // A slug is display metadata. Gen2 needs the canonical checkout owner.
        return Ok(vec![]);
    }
    ilex_tooling_windows::gen2::Provider::active(ws.identity()?)
}

#[cfg(target_os = "windows")]
fn image(core: core::Frame) -> Frame {
    Frame {
        width: core.width,
        height: core.height,
        rgb: core.pixels,
    }
}

#[cfg(target_os = "windows")]
fn scancode(key: &str) -> Result<u32> {
    let physical = match key {
        "esc" => 0x01,
        "backspace" => 0x0e,
        "tab" => 0x0f,
        "ret" => 0x1c,
        "ctrl" => 0x1d,
        "ctrl_r" => 0xe01d,
        "shift" => 0x2a,
        "shift_r" => 0x36,
        "alt" => 0x38,
        "alt_r" => 0xe038,
        "spc" => 0x39,
        "caps_lock" => 0x3a,
        "up" => 0xe048,
        "down" => 0xe050,
        "left" => 0xe04b,
        "right" => 0xe04d,
        "home" => 0xe047,
        "end" => 0xe04f,
        "pgup" => 0xe049,
        "pgdn" => 0xe051,
        "insert" => 0xe052,
        "delete" => 0xe053,
        "meta_l" => 0xe05b,
        "meta_r" => 0xe05c,
        "menu" => 0xe05d,
        "print" => 0xe037,
        "minus" => 0x0c,
        "equal" => 0x0d,
        "bracket_left" => 0x1a,
        "bracket_right" => 0x1b,
        "semicolon" => 0x27,
        "apostrophe" => 0x28,
        "grave_accent" => 0x29,
        "backslash" => 0x2b,
        "comma" => 0x33,
        "dot" => 0x34,
        "slash" => 0x35,
        "f11" => 0x57,
        "f12" => 0x58,
        function if function.starts_with('f') && function.len() > 1 => {
            let n: u32 = function[1..].parse()?;
            anyhow::ensure!((1..=10).contains(&n), "unsupported Gen2 function key");
            0x3a + n
        }
        _ => {
            for (row, start) in [
                ("1234567890", 0x02),
                ("qwertyuiop", 0x10),
                ("asdfghjkl", 0x1e),
                ("zxcvbnm", 0x2c),
            ] {
                if key.len() == 1
                    && let Some(n) = row.find(key)
                {
                    return Ok(start + n as u32);
                }
            }
            bail!("unsupported Gen2 key {key}")
        }
    };
    Ok(physical)
}
