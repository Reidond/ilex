//! WSLg, the part of WSL2 that shows Linux windows on the Windows desktop.
//!
//! Each time WSL starts, WSLg's compositor (Weston) tries to open a shared
//! memory area with Windows. When that fails (`rdp_allocate_shared_memory:
//! Failed to open "/mnt/shared_memory/…": Input/output error` in its log,
//! microsoft/WSL#40618; on this machine on most starts of WSL 2.7.x), Weston
//! falls back to "copy mode": every Linux window gets a taskbar button titled
//! `[WARN:COPY MODE] …` and is never drawn. Only a new start of WSL can fix it,
//! so `cargo xtask vm --gui` checks the log before it opens a window.

use std::fs;

/// Weston's log, rewritten on every start of WSLg.
pub const LOG: &str = "/mnt/wslg/weston.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Windows get their pixels through shared memory.
    Healthy,
    /// Copy mode: windows stay invisible until WSL restarts.
    Broken,
    /// Weston hasn't logged its RDP backend settings yet.
    Starting,
}

/// Read the state from the text of [`LOG`].
pub fn state_from_log(log: &str) -> State {
    if log.contains("rdp_allocate_shared_memory: Failed") || log.contains("use_gfxredir = 0") {
        State::Broken
    } else if log.contains("use_gfxredir = 1") {
        State::Healthy
    } else {
        State::Starting
    }
}

/// WSLg's state when this Linux host is WSL with WSLg, `None` elsewhere.
pub fn local_state() -> Option<State> {
    fs::read_to_string(LOG).ok().map(|log| state_from_log(&log))
}

/// What to tell someone whose WSLg is in copy mode.
pub const BROKEN_HELP: &str = "WSLg can't show windows in this start of WSL: its shared memory failed \
     (/mnt/wslg/weston.log, microsoft/WSL#40618), so a window would only get an invisible \
     `[WARN:COPY MODE]` taskbar button. Run `cargo xtask vm --gui` from Windows instead (it restarts \
     WSL until WSLg is healthy), or run `wsl --shutdown` and try again";
