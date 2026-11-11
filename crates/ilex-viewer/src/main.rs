//! `ilex-viewer`: the native Windows window of `cargo xtask vm --gui`.
//!
//! It connects to the display bridge `xtask vm` runs in WSL
//! (`ilex_devkit::display`), shows the guest's screen, and sends the
//! keyboard, pointer and clipboard back. The keyboard goes through a
//! low-level hook while the window has focus, so the Windows key, Alt+Tab
//! and the like reach GNOME instead of Windows. `cargo xtask vm --gui` on
//! Windows starts it; by hand:
//!
//! ```text
//! ilex-viewer --connect 172.17.96.188:41234 --token <hex>
//! ```

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod keyboard;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "ilex-viewer", about = "Native Windows window for an ilex VM")]
pub struct Args {
    /// The bridge's address, as `xtask vm --gui` prints it
    #[arg(long, required_unless_present = "keyboard_hook")]
    pub connect: Option<String>,
    /// The bridge's token, as `xtask vm --gui` prints it
    #[arg(long, required_unless_present = "keyboard_hook")]
    pub token: Option<String>,
    /// Window title
    #[arg(long, default_value = "ilex VM")]
    pub title: String,
    /// Internal: be the keyboard hook process for the viewer window with
    /// this handle (see keyboard.rs)
    #[arg(long, hide = true)]
    pub keyboard_hook: Option<usize>,
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args = Args::parse();
    if let Some(hwnd) = args.keyboard_hook {
        keyboard::hook_process(hwnd);
        return std::process::ExitCode::SUCCESS;
    }
    match app::run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ilex-viewer: error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    let _ = Args::parse();
    eprintln!(
        "ilex-viewer is a Windows program; on Linux, `cargo xtask vm --gui` opens a GTK window instead"
    );
    std::process::ExitCode::FAILURE
}
