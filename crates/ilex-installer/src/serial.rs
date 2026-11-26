//! Marker lines for the test harness and for whoever watches a serial
//! console. Written to the journal (stdout) and, best effort, to `/dev/ttyS0`.
//!
//! The serial write never delays the interface. Opening the port non-blocking
//! and writing once still leaves `close()`: a UART driver drains its output
//! there and waits up to its `closing_wait` (30 seconds by default) when the
//! line is flow-controlled. So the port is only ever touched by a disposable
//! worker, which the caller waits for at most [`BUDGET`], as
//! `installer/chooser/diagnostics.py` does for the Anaconda chooser: a worker
//! stuck in `close()` is abandoned (it holds nothing the interface needs), and
//! further markers are skipped, not queued behind it, until it is gone.
//!
//! A fixed public test credential, `ilex.installer.serial-test`, makes the
//! mirror fail on purpose (`unavailable`: no such device; `backpressure`: a
//! pseudo terminal whose output is stopped; `stall`: the worker hangs for
//! three seconds, as a UART that never drains would hang it in `close()`). The
//! outcome of each faulted write is reported on the second UART, `/dev/ttyS1`,
//! so the harness can observe it independently of the sink being exercised.
//! The credential can only damage the serial mirror.

use std::io::Write;

/// What the interface waits for the worker, and for it to be reaped.
#[cfg(target_os = "linux")]
const BUDGET: std::time::Duration = std::time::Duration::from_millis(200);
/// One attempt writes at most this many bytes.
#[cfg(target_os = "linux")]
const MAX_BYTES: usize = 4096;
#[cfg(target_os = "linux")]
const CONSOLE: &str = "/dev/ttyS0";
#[cfg(target_os = "linux")]
const AUDIT_CONSOLE: &str = "/dev/ttyS1";
/// The fault credential, read from the service's credentials directory.
#[cfg(target_os = "linux")]
const CREDENTIAL: &str = "ilex.installer.serial-test";

/// Print one marker line to the journal and mirror it to the serial console.
pub fn marker(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
    let _ = std::io::stdout().flush();
    #[cfg(target_os = "linux")]
    linux::mirror(line);
}

#[cfg(target_os = "linux")]
fn line_payload(line: &str) -> Vec<u8> {
    let mut payload = line.as_bytes()[..line.len().min(MAX_BYTES - 1)].to_vec();
    payload.push(b'\n');
    payload
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{ErrorKind, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use super::{AUDIT_CONSOLE, BUDGET, CONSOLE, CREDENTIAL, line_payload};

    /// A worker that holds a console is still alive: skip, never queue. One
    /// flag per console, so a stuck mirror cannot silence the audit UART.
    static BUSY: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

    #[derive(Clone, Copy, PartialEq, Debug)]
    pub enum Status {
        Written,
        Partial,
        Backpressure,
        Unavailable,
        Error,
        /// The worker did not finish within [`BUDGET`].
        Budget,
        /// An earlier worker still holds the port.
        Busy,
    }

    impl Status {
        pub fn name(self) -> &'static str {
            match self {
                Status::Written => "written",
                Status::Partial => "partial",
                Status::Backpressure => "backpressure",
                Status::Unavailable => "unavailable",
                Status::Error => "error",
                Status::Budget => "budget",
                Status::Busy => "busy",
            }
        }
    }

    /// The test fault named by the credential, if any (`unavailable`,
    /// `backpressure` or `stall`; anything else is no fault).
    fn fault() -> Option<&'static str> {
        static FAULT: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
        *FAULT.get_or_init(|| {
            let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
            let text = std::fs::read_to_string(Path::new(&dir).join(CREDENTIAL)).ok()?;
            match text.trim() {
                "unavailable" => Some("unavailable"),
                "backpressure" => Some("backpressure"),
                "stall" => Some("stall"),
                _ => None,
            }
        })
    }

    pub fn mirror(line: &str) {
        let fault = fault();
        let started = Instant::now();
        let status = attempt_with_deadline(0, Path::new(CONSOLE), fault, line_payload(line));
        if let Some(mode) = fault {
            // Observed through the other UART, never through the faulted sink.
            let first = line.split_whitespace().next().unwrap_or("");
            let audit = format!(
                "{line}\nILEX-INSTALLER-SERIAL mode={mode} status={} seconds={:.3} message={first}",
                status.name(),
                started.elapsed().as_secs_f64()
            );
            attempt_with_deadline(1, Path::new(AUDIT_CONSOLE), None, line_payload(&audit));
        }
    }

    /// One write on a disposable worker, awaited for at most [`BUDGET`].
    /// `console` selects the busy flag (0: the mirror, 1: the audit UART).
    pub fn attempt_with_deadline(
        console: usize,
        path: &Path,
        fault: Option<&str>,
        payload: Vec<u8>,
    ) -> Status {
        if BUSY[console].swap(true, Ordering::AcqRel) {
            return Status::Busy;
        }
        let (done, result) = mpsc::channel();
        let path = path.to_owned();
        let fault = fault.map(str::to_owned);
        let spawned = std::thread::Builder::new()
            .name("serial-marker".into())
            .spawn(move || {
                // `attempt` has closed the port by now: the port is free again
                // before the caller hears of it (or has stopped waiting).
                let status = attempt(&path, fault.as_deref(), &payload);
                BUSY[console].store(false, Ordering::Release);
                let _ = done.send(status);
            });
        if spawned.is_err() {
            BUSY[console].store(false, Ordering::Release);
            return Status::Error;
        }
        result.recv_timeout(BUDGET).unwrap_or(Status::Budget)
    }

    /// Open non-blocking, write once, close: no retry, drain or flush, and
    /// the port's termios stay as they are.
    fn attempt(path: &Path, fault: Option<&str>, payload: &[u8]) -> Status {
        // Held for the duration of the write when the fault is a stopped pty.
        let mut held: Vec<OwnedFd> = Vec::new();
        if fault == Some("stall") {
            // A UART that never drains: the worker is stuck for good, as far
            // as the caller can tell.
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let path = match fault {
            // Under /dev, which every user can search: a path below the
            // helper's 0700 runtime directory would fail with EACCES (an
            // "error"), not with ENOENT.
            Some("unavailable") => Path::new("/dev/ilex-installer-no-such-console").to_owned(),
            Some("backpressure") => match stopped_pty(&mut held) {
                Some(slave) => slave,
                None => return Status::Error,
            },
            _ => path.to_owned(),
        };
        let mut port = match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(port) => port,
            Err(error) => return classify(&error),
        };
        match port.write(payload) {
            Ok(n) if n == payload.len() => Status::Written,
            Ok(_) => Status::Partial,
            Err(error) => classify(&error),
        }
        // `port` closes here: this is the call that may wait for the UART.
    }

    fn classify(error: &std::io::Error) -> Status {
        match error.kind() {
            ErrorKind::WouldBlock => Status::Backpressure,
            ErrorKind::NotFound => Status::Unavailable,
            _ => match error.raw_os_error() {
                Some(libc::ENODEV | libc::ENXIO) => Status::Unavailable,
                _ => Status::Error,
            },
        }
    }

    /// A pseudo terminal whose output is stopped (`tcflow(TCOOFF)`): its
    /// write room is zero, so a non-blocking write gets EAGAIN. Returns the
    /// slave's path; both ends stay open in `held`.
    fn stopped_pty(held: &mut Vec<OwnedFd>) -> Option<std::path::PathBuf> {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: plain libc calls on descriptors this function creates and
        // immediately owns.
        unsafe {
            if libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            ) != 0
            {
                return None;
            }
            held.push(OwnedFd::from_raw_fd(master));
            held.push(OwnedFd::from_raw_fd(slave));
            if libc::tcflow(slave, libc::TCOOFF) != 0 {
                return None;
            }
            let mut name = [0 as libc::c_char; 128];
            if libc::ttyname_r(held[1].as_raw_fd(), name.as_mut_ptr(), name.len()) != 0 {
                return None;
            }
            let name = std::ffi::CStr::from_ptr(name.as_ptr());
            Some(std::path::PathBuf::from(name.to_str().ok()?))
        }
    }
}
