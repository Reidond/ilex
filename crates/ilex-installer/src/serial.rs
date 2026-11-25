//! Marker lines for the test harness and for whoever watches a serial
//! console. Written to the journal (stdout) and, best effort, to `/dev/ttyS0`.

use std::io::Write;

/// Print one marker line. The serial write never blocks the interface: the
/// port is opened non-blocking, written once and skipped when it is missing,
/// not writable or backed up.
pub fn marker(line: &str) {
    println!("{line}");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut port) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/ttyS0")
        {
            let _ = port.write(format!("{line}\n").as_bytes());
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = std::io::stdout().flush();
}
