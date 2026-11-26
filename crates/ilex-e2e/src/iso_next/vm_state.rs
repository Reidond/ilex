//! What a booted installer VM is doing when it has printed nothing for its
//! whole deadline: the report a failed `iso::next` trial writes beside its
//! serial log.
//!
//! An empty `serial.log` says only that no marker came. The report tells a
//! guest that never left the firmware apart from one that is paused (QMP
//! status `internal-error`, a KVM entry failure in `qemu.log`), one that is
//! halted waiting for an event that never arrives, and one that is busy:
//! per-thread CPU time of the QEMU process over a short window (a vCPU thread
//! that burns a core is running, one that does not is idle), the vCPUs'
//! registers over QMP, the size and age of both serial files, the firmware's
//! own log when the VM had one, and the host's load, pressure and steal at that
//! moment.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use ilex_devkit::qemu::Vm;
use ilex_devkit::qmp::Qmp;
use serde_json::json;

/// The window over which per-thread CPU time is measured.
const SAMPLE_WINDOW: Duration = Duration::from_secs(2);

/// A text report about `vm`. Every part that cannot be read says so; the
/// report itself never fails (it runs on a failure path).
pub(super) fn report(vm: &mut Vm) -> String {
    let mut out = String::new();
    let info = &vm.info;
    let _ = writeln!(
        out,
        "=== {} (qemu pid {}, ssh port {}, secure boot {})",
        info.name, info.pid, info.ssh_port, info.secure_boot
    );
    let _ = writeln!(
        out,
        "exited: {}",
        match vm.exited() {
            Ok(None) => "no".to_string(),
            Ok(Some(status)) => status.to_string(),
            Err(e) => format!("unknown ({e})"),
        }
    );

    let _ = writeln!(out, "=== QMP");
    match Qmp::connect_with_timeout(&vm.info.qmp_sock, Duration::from_secs(5)) {
        Ok(mut qmp) => {
            for command in ["query-status", "query-cpus-fast"] {
                match qmp.execute(command, json!({})) {
                    Ok(value) => {
                        let _ = writeln!(out, "{command}: {value}");
                    }
                    Err(e) => {
                        let _ = writeln!(out, "{command}: {e:#}");
                    }
                }
            }
            match qmp.execute(
                "human-monitor-command",
                json!({"command-line": "info registers -a"}),
            ) {
                Ok(value) => {
                    // The instruction pointer and halt state of each CPU; the
                    // rest of the dump is floating-point state.
                    let text = value.as_str().unwrap_or_default();
                    let _ = writeln!(out, "info registers -a (instruction pointers):");
                    for line in text
                        .lines()
                        .filter(|l| l.starts_with("CPU#") || l.starts_with("RIP="))
                    {
                        let _ = writeln!(out, "  {line}");
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "info registers -a: {e:#}");
                }
            }
        }
        Err(e) => {
            let _ = writeln!(out, "no QMP connection: {e:#}");
        }
    }

    let _ = writeln!(out, "=== qemu threads (CPU ticks over {SAMPLE_WINDOW:?})");
    let first = threads(vm.info.pid);
    std::thread::sleep(SAMPLE_WINDOW);
    let second = threads(vm.info.pid);
    for (tid, comm, state, ticks) in &second {
        let before = first
            .iter()
            .find(|(t, ..)| t == tid)
            .map_or(0, |(.., ticks)| *ticks);
        let _ = writeln!(
            out,
            "{tid:>7} {comm:<20} {state} cpu {}% total {ticks} ticks",
            (ticks - before) * 100 / (SAMPLE_WINDOW.as_secs() * clock_ticks()).max(1)
        );
    }
    if second.is_empty() {
        let _ = writeln!(out, "(no threads: the QEMU process is gone)");
    }
    // Where each thread sleeps: the kernel's view (readable as root), and a
    // user-space backtrace when gdb is installed. A QEMU that never reached its
    // main loop shows what it waits for here.
    let _ = writeln!(out, "=== qemu thread wait points");
    for (tid, comm, ..) in &second {
        let read = |name: &str| {
            fs::read_to_string(format!("/proc/{}/task/{tid}/{name}", vm.info.pid)).map_or_else(
                |e| format!("({e})"),
                |text| text.trim().replace('\n', " | "),
            )
        };
        let _ = writeln!(
            out,
            "{tid:>7} {comm:<20} wchan {} syscall {} stack {}",
            read("wchan"),
            read("syscall"),
            read("stack")
        );
    }
    if let Ok(backtrace) = Command::new("timeout")
        .args(["60", "gdb", "-p", &vm.info.pid.to_string(), "-batch"])
        .args(["-ex", "thread apply all bt 16"])
        .output()
    {
        let _ = writeln!(
            out,
            "gdb backtrace:\n{}{}",
            String::from_utf8_lossy(&backtrace.stdout),
            String::from_utf8_lossy(&backtrace.stderr)
        );
    }

    let _ = writeln!(out, "=== guest consoles");
    for name in ["serial.log", "serial-secondary.log", "firmware.log"] {
        let path = vm.info.dir.join(name);
        match fs::metadata(&path) {
            Ok(meta) => {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .map_or("unknown".to_string(), |d| format!("{}s ago", d.as_secs()));
                let _ = writeln!(out, "{name}: {} bytes, last written {age}", meta.len());
            }
            Err(_) => {
                let _ = writeln!(out, "{name}: absent");
            }
        }
    }
    if let Ok(bytes) = fs::read(vm.info.dir.join("firmware.log")) {
        let text = String::from_utf8_lossy(&bytes);
        let tail: Vec<&str> = text.lines().rev().take(40).collect();
        let _ = writeln!(
            out,
            "firmware.log, last {} lines:\n{}",
            tail.len(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    let _ = writeln!(out, "=== qemu.log");
    let _ = writeln!(
        out,
        "{}",
        fs::read_to_string(vm.info.dir.join("qemu.log")).unwrap_or_else(|e| format!("({e})"))
    );

    let _ = writeln!(out, "=== host");
    out.push_str(&host());
    out
}

/// `(tid, name, state, utime+stime)` of each thread of `pid`.
fn threads(pid: u32) -> Vec<(String, String, char, u64)> {
    let Ok(dir) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut threads: Vec<_> = dir
        .flatten()
        .filter_map(|entry| {
            let tid = entry.file_name().to_string_lossy().into_owned();
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            // `pid (comm) S ppid …`: comm may hold spaces and parentheses.
            let (head, rest) = stat.rsplit_once(')')?;
            let comm = head.split_once('(')?.1.to_owned();
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let state = fields.first()?.chars().next()?;
            let ticks =
                fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
            Some((tid, comm, state, ticks))
        })
        .collect();
    threads.sort_by_key(|(tid, ..)| tid.parse::<u64>().unwrap_or(0));
    threads
}

/// Scheduler ticks per second (`CLK_TCK`, 100 on every Linux the harness runs on).
fn clock_ticks() -> u64 {
    100
}

/// The host at this moment: load, pressure stalls, CPU and steal time over a
/// second, memory, and what else is running QEMU.
pub(super) fn host() -> String {
    let mut out = String::new();
    let read = |path: &str| fs::read_to_string(path).unwrap_or_else(|e| format!("({path}: {e})\n"));
    let _ = writeln!(out, "loadavg: {}", read("/proc/loadavg").trim());
    for name in ["cpu", "io", "memory"] {
        let _ = write!(
            out,
            "pressure/{name}: {}",
            read(&format!("/proc/pressure/{name}"))
        );
    }
    let cpu = |text: &str| -> Vec<u64> {
        text.lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .skip(1)
            .filter_map(|n| n.parse().ok())
            .collect()
    };
    let before = cpu(&read("/proc/stat"));
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let after = cpu(&read("/proc/stat"));
    if before.len() >= 8 && after.len() >= 8 {
        let delta: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
        let total: u64 = delta.iter().take(8).sum::<u64>().max(1);
        let pct = |n: u64| n * 100 / total;
        let _ = writeln!(
            out,
            "cpu over {:.1}s: user {}% nice {}% system {}% idle {}% iowait {}% irq {}% softirq {}% steal {}%",
            started.elapsed().as_secs_f64(),
            pct(delta[0]),
            pct(delta[1]),
            pct(delta[2]),
            pct(delta[3]),
            pct(delta[4]),
            pct(delta[5]),
            pct(delta[6]),
            pct(delta[7])
        );
    }
    for line in read("/proc/meminfo").lines().filter(|l| {
        [
            "MemTotal",
            "MemFree",
            "MemAvailable",
            "Cached",
            "Dirty",
            "Writeback",
            "SwapFree",
        ]
        .iter()
        .any(|k| l.starts_with(k))
    }) {
        let _ = writeln!(out, "meminfo {line}");
    }
    let _ = writeln!(out, "nproc: {:?}", std::thread::available_parallelism());
    let qemus = qemu_processes();
    let _ = writeln!(out, "qemu processes running: {}", qemus.len());
    for line in qemus {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// `pid state rss name` of every running `qemu-system-*`.
fn qemu_processes() -> Vec<String> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            if !comm.starts_with("qemu-system") {
                return None;
            }
            let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
            let words: Vec<String> = cmdline
                .split(|b| *b == 0)
                .map(|w| String::from_utf8_lossy(w).into_owned())
                .collect();
            let name = words
                .iter()
                .position(|w| w == "-name")
                .and_then(|i| words.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let rss = super::process_rss_kib(pid).unwrap_or(0);
            Some(format!("{pid} {name} rss {rss} KiB"))
        })
        .collect()
}

/// Where a report is written.
pub(super) fn write(artifacts: &Path, name: &str, report: &str) {
    let _ = fs::write(
        artifacts.join(format!("next-{name}-qemu-state.txt")),
        report,
    );
}
