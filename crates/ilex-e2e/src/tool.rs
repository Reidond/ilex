//! `tool::` tier: drupe, developer tools from signed disk images.
//!
//! One ephemeral VM. Booting it builds the test tool (`strace`, from the
//! Fedora package, as `cargo xtask tool strace` does; reused once built,
//! `ILEX_TOOL_REBUILD=1` builds it again), shares the image with the guest,
//! tries to install it before its certificate is trusted, then trusts the
//! certificate (`/run/verity.d`) and installs it. The tests then run the
//! tool as an unprivileged user and look at what it can see.
//!
//! The tests share the VM and run in parallel; the one that removes the tool
//! takes [`STATE`] exclusively.
//!
//! An ephemeral VM has SELinux disabled, no composefs root and no reboot, so
//! `tool::installed_system` boots the installed disk of the `disk::` tier
//! instead (a VM of its own, `tool-disk`, with the tool image attached as a
//! second drive): the tool under SELinux enforcing, and `drupe.service`
//! bringing it back after a reboot.

use std::fs;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ilex_devkit::bcvk::{EphemeralOpts, EphemeralVm};
use ilex_devkit::qemu::{Vm, VmConfig};
use ilex_devkit::{Workspace, tool};
use libtest_mimic::Trial;

use crate::common::{self, Group, ensure, trial};
use crate::disk::vmtest::{self, Report};

/// The tool under test, and the package it is built from.
const TOOL: &str = "strace";
/// Where the guest sees `<state dir>/tools/images` (`--ro-bind …:tools`).
const SHARE: &str = "/run/virtiofs-mnt-tools";
/// The setuid helper, called directly where a test needs to run something
/// other than the tool inside its mount namespace.
const ENTER: &str = "/usr/libexec/drupe/drupe-enter";

static GROUP: Group<ToolVm> = Group::new("tool", boot);
/// Read: the tool is installed and stays so. Write: a test changes that.
static STATE: RwLock<()> = RwLock::new(());

struct ToolVm {
    vm: EphemeralVm,
    /// Exit status and output of `drupe install` before the certificate was
    /// trusted.
    untrusted: (i32, String),
}

fn boot() -> Result<ToolVm> {
    let ws = common::workspace()?;
    let image = common::image()?;
    test_tool(&ws)?;

    let name = ws.vm_name("tool");
    let opts = EphemeralOpts {
        extra_args: vec![
            "--ro-bind".into(),
            format!("{}:tools", tool::images_dir(&ws).display()),
        ],
        ..EphemeralOpts::default()
    };
    let vm = EphemeralVm::boot(&name, &image, &opts)?;
    eprintln!("ilex: {name} is up from {image} (VM slot {})", vm.slot_index());
    vm.ssh_timeout("systemctl is-system-running --wait", Duration::from_secs(300))?;

    let install = format!("drupe install {SHARE}/{TOOL}.raw");
    let untrusted = sh(&vm, &install)?;
    sh_ok(
        &vm,
        &format!("mkdir -p /run/verity.d && cp {SHARE}/drupe.crt /run/verity.d/drupe.crt"),
    )?;
    sh_ok(&vm, &install)?;
    Ok(ToolVm { vm, untrusted })
}

/// The test tool's image, built on first use. Both VMs of the tier want it,
/// possibly at the same moment: one build at a time.
fn test_tool(ws: &Workspace) -> Result<tool::Built> {
    static BUILDING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = BUILDING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut built = tool::built(ws, TOOL);
    let rebuild = std::env::var_os("ILEX_TOOL_REBUILD").is_some_and(|v| !v.is_empty());
    if rebuild || !built.image.is_file() || !built.certificate.is_file() {
        built = tool::build(ws, TOOL, &[TOOL.to_string()])?;
    }
    eprintln!("ilex: tool image {}", built.image.display());
    Ok(built)
}

pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    let shared = |name: &'static str, body: fn(&ToolVm) -> Result<()>| {
        trial(name, move || {
            GROUP.run(|t| {
                let _installed = STATE.read().unwrap_or_else(PoisonError::into_inner);
                body(t)
            })
        })
    };
    vec![
        shared("tool::untrusted_image_refused", untrusted_image_refused),
        shared("tool::listed", listed),
        shared("tool::runs_as_the_user", runs_as_the_user),
        shared("tool::traces_a_host_program", traces_a_host_program),
        shared("tool::attaches_to_a_running_process", attaches_to_a_running_process),
        shared("tool::environment_reaches_the_tool", environment_reaches_the_tool),
        shared("tool::app_is_private", app_is_private),
        shared("tool::path_prefers_tools", path_prefers_tools),
        shared("tool::bad_requests_refused", bad_requests_refused),
        shared("tool::damaged_image_refused", damaged_image_refused),
        trial("tool::remove_and_reinstall", || {
            GROUP.run(|t| {
                let _exclusive = STATE.write().unwrap_or_else(PoisonError::into_inner);
                remove_and_reinstall(t)
            })
        }),
        trial("tool::installed_system", installed_system),
    ]
}

/// Run `script` in the guest's shell; returns its exit status and everything
/// it printed.
///
/// `bcvk ephemeral ssh` gives the guest a terminal and reports every failure
/// as exit 1, so: stderr arrives on stdout anyway (merged here on purpose),
/// the status travels in a last line of its own, and the output goes through
/// `cat` so that no program sees a terminal and colours it (strace does).
fn sh(vm: &EphemeralVm, script: &str) -> Result<(i32, String)> {
    let out = vm.ssh(&format!(
        "set -o pipefail; {{ {script}; }} 2>&1 | cat; echo \"rc=$?\""
    ))?;
    let text = out.stdout.replace('\r', "");
    let parsed = text
        .trim_end()
        .rsplit_once("rc=")
        .and_then(|(body, rc)| Some((rc.parse().ok()?, body.to_string())));
    parsed.with_context(|| {
        format!("no exit status from `{script}`:\n{}{}", out.stdout, out.stderr)
    })
}

/// [`sh`], failing unless the script exited 0.
fn sh_ok(vm: &EphemeralVm, script: &str) -> Result<String> {
    let (rc, text) = sh(vm, script)?;
    ensure(rc == 0, || format!("`{script}` exited {rc}:\n{text}"))?;
    Ok(text)
}

/// `cmd` as an unprivileged user (no shell in between).
fn as_user(cmd: &str) -> String {
    format!("runuser -u nobody -- {cmd}")
}

/// An image whose signature no trusted certificate verifies is not
/// installed, however intact it is.
fn untrusted_image_refused(t: &ToolVm) -> Result<()> {
    let (rc, text) = &t.untrusted;
    ensure(*rc != 0, || {
        "drupe installed an image before its certificate was trusted".to_string()
    })?;
    ensure(text.contains("lacks recognized signature"), || {
        format!("expected a signature error, got:\n{text}")
    })
}

/// `drupe list` (anyone may ask) names the tool and its commands, each
/// command is a symlink to drupe in the PATH directory, and the mounted
/// images are closed to users.
fn listed(t: &ToolVm) -> Result<()> {
    let list = sh_ok(&t.vm, &as_user("drupe list"))?;
    let line = list.lines().find(|l| l.starts_with("strace\t"));
    ensure(
        line.is_some_and(|l| l.ends_with("\tstrace strace-log-merge")),
        || format!("unexpected `drupe list`:\n{list}"),
    )?;
    let link = sh_ok(&t.vm, "readlink /run/drupe/bin/strace")?;
    ensure(link.trim() == "/usr/bin/drupe", || {
        format!("/run/drupe/bin/strace points to {link}")
    })?;
    let mode = sh_ok(&t.vm, "stat -c %a /run/drupe/priv")?;
    ensure(mode.trim() == "700", || {
        format!("/run/drupe/priv has mode {mode}")
    })?;
    let (rc, _) = sh(&t.vm, &as_user("ls /run/drupe/priv/strace"))?;
    ensure(rc != 0, || {
        "a user can read the mounted image outside a tool's namespace".to_string()
    })
}

/// What runs inside the tool's namespace is the caller in every id, with no
/// capabilities left from the setuid helper, and still able to gain some:
/// no_new_privs is off, so the host's `sudo` gets as far as asking for a
/// password.
fn runs_as_the_user(t: &ToolVm) -> Result<()> {
    let uid = sh_ok(&t.vm, "id -u nobody")?.trim().to_string();
    let gid = sh_ok(&t.vm, "id -g nobody")?.trim().to_string();
    let status = sh_ok(
        &t.vm,
        &as_user(&format!("{ENTER} {TOOL} - /usr/bin/cat cat /proc/self/status")),
    )?;
    let field = |key: &str| -> Vec<&str> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.split_whitespace().collect())
            .unwrap_or_default()
    };
    for (key, id) in [("Uid:", &uid), ("Gid:", &gid)] {
        let ids = field(key);
        ensure(ids.len() == 4 && ids.iter().all(|i| i == id), || {
            format!("{key} is {ids:?}, expected four times {id}")
        })?;
    }
    for key in ["CapPrm:", "CapEff:", "CapInh:", "CapAmb:"] {
        ensure(field(key) == ["0000000000000000"], || {
            format!("{key} is {:?}, expected none", field(key))
        })?;
    }
    ensure(field("NoNewPrivs:") == ["0"], || {
        format!("NoNewPrivs is {:?}", field("NoNewPrivs:"))
    })?;
    let (_, sudo) = sh(
        &t.vm,
        &as_user(&format!("{ENTER} {TOOL} - /usr/bin/sudo sudo -n true")),
    )?;
    ensure(sudo.contains("a password is required"), || {
        format!("sudo in the tool's namespace did not get to authentication:\n{sudo}")
    })
}

/// The point of it all: the tool comes from its image (the host has no
/// strace), and the program it starts is the host's, loading the host's
/// libraries, not the ones the tool brought.
fn traces_a_host_program(t: &ToolVm) -> Result<()> {
    let (_, host) = sh(&t.vm, "ls /usr/bin/strace /usr/sbin/strace 2>/dev/null")?;
    ensure(host.trim().is_empty(), || {
        format!(
            "the image has its own strace ({}): pick another test tool",
            host.trim()
        )
    })?;
    let trace = sh_ok(
        &t.vm,
        &as_user("/run/drupe/bin/strace -e trace=execve,openat /usr/bin/true"),
    )?;
    for want in [
        "execve(\"/usr/bin/true\"",
        "openat(AT_FDCWD, \"/lib64/libc.so.6\"",
        "+++ exited with 0 +++",
    ] {
        ensure(trace.contains(want), || {
            format!("no `{want}` in the trace:\n{trace}")
        })?;
    }
    ensure(!trace.contains("/app/"), || {
        format!("the traced program opened something in /app:\n{trace}")
    })?;
    // The version it reports is the image's.
    let version = sh_ok(&t.vm, &as_user("drupe run strace -V"))?;
    let list = sh_ok(&t.vm, "drupe list")?;
    let packaged = list
        .lines()
        .find_map(|l| l.strip_prefix("strace\t"))
        .and_then(|l| l.split(['-', '\t']).next())
        .unwrap_or_default()
        .to_string();
    ensure(
        !packaged.is_empty() && version.contains(&format!("version {packaged}")),
        || format!("strace -V says:\n{version}\nbut the image holds `{packaged}`"),
    )
}

/// The tool sees the host's processes: it attaches to one it did not start,
/// which is what fails from a container or a user namespace. As root,
/// because the image's `kernel.yama.ptrace_scope=1` lets a user attach to
/// their own descendants only, whichever strace they use.
fn attaches_to_a_running_process(t: &ToolVm) -> Result<()> {
    let (_, out) = sh(
        &t.vm,
        "sleep 30 & pid=$!; timeout 3 /run/drupe/bin/strace -p $pid; kill $pid",
    )?;
    ensure(out.contains("attached"), || {
        format!("strace did not attach:\n{out}")
    })
}

/// The caller's environment arrives whole, including what glibc removes
/// from a setuid program's (the helper's) own environment.
fn environment_reaches_the_tool(t: &ToolVm) -> Result<()> {
    let out = sh_ok(
        &t.vm,
        &as_user(
            "env LD_LIBRARY_PATH=/nonexistent TMPDIR=/var/tmp DRUPE_TEST=1 \
             /run/drupe/bin/strace -o /dev/null /usr/bin/env",
        ),
    )?;
    for want in ["LD_LIBRARY_PATH=/nonexistent", "TMPDIR=/var/tmp", "DRUPE_TEST=1"] {
        ensure(out.lines().any(|l| l == want), || {
            format!("`{want}` did not reach the traced program:\n{out}")
        })?;
    }
    Ok(())
}

/// Inside a tool's namespace `/app` is its image: read-only, and without
/// setuid programs or device nodes whatever the image holds. Outside, `/app`
/// stays an empty directory: the mount never leaves the namespace.
fn app_is_private(t: &ToolVm) -> Result<()> {
    let inside = sh_ok(
        &t.vm,
        &as_user(&format!(
            "{ENTER} {TOOL} - /usr/bin/findmnt findmnt -no FSTYPE,OPTIONS /app"
        )),
    )?;
    let mut fields = inside.split_whitespace();
    let fstype = fields.next().unwrap_or_default();
    let options: Vec<&str> = fields.next().unwrap_or_default().split(',').collect();
    ensure(
        fstype == "erofs" && ["ro", "nosuid", "nodev"].iter().all(|o| options.contains(o)),
        || format!("/app in the namespace is `{}`", inside.trim()),
    )?;

    let (_, outside) = sh(&t.vm, "findmnt -no SOURCE /app; ls -A /app")?;
    ensure(outside.trim().is_empty(), || {
        format!("/app is not empty on the host:\n{outside}")
    })
}

/// A login shell finds the tool's command first.
fn path_prefers_tools(t: &ToolVm) -> Result<()> {
    let out = sh_ok(&t.vm, "bash -lc 'command -v strace'")?;
    ensure(out.trim() == "/run/drupe/bin/strace", || {
        format!("a login shell resolves strace to `{}`", out.trim())
    })
}

/// Unknown commands and tools fail like a shell would, and the helper takes
/// nothing but a plain tool name and an absolute program.
fn bad_requests_refused(t: &ToolVm) -> Result<()> {
    for (cmd, code) in [
        ("drupe run no-such-command".to_string(), 127),
        (format!("{ENTER} no-such-tool - /usr/bin/true true"), 126),
        (format!("{ENTER} ../../etc - /usr/bin/true true"), 126),
        (format!("{ENTER} {TOOL} - true true"), 126),
    ] {
        let (rc, text) = sh(&t.vm, &as_user(&cmd))?;
        ensure(rc == code, || {
            format!("`{cmd}` exited {rc}, expected {code}:\n{text}")
        })?;
    }
    Ok(())
}

/// dm-verity catches a changed block when it is read; `drupe install` reads
/// them all, so a damaged image is refused and the installed tool stays.
fn damaged_image_refused(t: &ToolVm) -> Result<()> {
    let (rc, text) = sh(
        &t.vm,
        &format!(
            "cp {SHARE}/{TOOL}.raw /var/tmp/damaged.raw && \
             dd if=/dev/urandom of=/var/tmp/damaged.raw bs=1M seek=3 count=1 conv=notrunc status=none && \
             drupe install /var/tmp/damaged.raw; rc=$?; rm -f /var/tmp/damaged.raw; (exit $rc)"
        ),
    )?;
    ensure(rc != 0 && text.contains("the image is damaged"), || {
        format!("a damaged image was not refused (exit {rc}):\n{text}")
    })?;
    sh_ok(&t.vm, &as_user("drupe run strace -V"))?;
    Ok(())
}

/// `drupe.service` (what a boot runs) is happy with an already active tool;
/// removing a tool takes its commands, its mount and its image away;
/// installing it again brings it back.
fn remove_and_reinstall(t: &ToolVm) -> Result<()> {
    sh_ok(&t.vm, "systemctl restart drupe.service")?;
    let unit = sh_ok(&t.vm, "systemctl is-active drupe.service")?;
    ensure(unit.trim() == "active", || {
        format!("drupe.service is `{}`", unit.trim())
    })?;
    sh_ok(&t.vm, &as_user("drupe run strace -V"))?;

    sh_ok(&t.vm, "drupe remove strace")?;
    let (_, left) = sh(
        &t.vm,
        "drupe list; \
         find /run/drupe/bin /run/drupe/cmd /run/drupe/tools /run/drupe/priv \
              /var/lib/drupe/images -mindepth 1; \
         findmnt -no TARGET /run/drupe/priv/strace; \
         losetup --list --noheadings --output BACK-FILE | grep drupe",
    )?;
    ensure(left.trim().is_empty(), || {
        format!("left behind after `drupe remove`:\n{left}")
    })?;
    let (rc, text) = sh(&t.vm, &as_user("drupe run strace -V"))?;
    ensure(rc == 127, || {
        format!("strace after its removal exited {rc}:\n{text}")
    })?;

    sh_ok(&t.vm, &format!("drupe install {SHARE}/{TOOL}.raw"))?;
    sh_ok(&t.vm, &as_user("drupe run strace -V"))?;
    Ok(())
}

/// The tool on an installed machine: composefs root, SELinux enforcing, a
/// real boot. The image arrives as a second drive (a copy: QEMU opens it
/// read-write) and its certificate goes to `/etc/verity.d`, which survives
/// the reboot like the installed image in `/var` does.
fn installed_system() -> Result<()> {
    let ws = common::workspace()?;
    let built = test_tool(&ws)?;
    let certificate = fs::read_to_string(&built.certificate)
        .with_context(|| format!("reading {}", built.certificate.display()))?;
    let drive = ws.state_dir().join("tools").join("tool-disk.raw");
    fs::copy(&built.image, &drive).with_context(|| format!("writing {}", drive.display()))?;

    let disk = vmtest::disk(&ws)?;
    let mut config = VmConfig::new("tool-disk", disk);
    config.extra_disks = vec![drive.clone()];
    let mut vm = vmtest::boot(&ws, config)?;

    let mut report = Report::default();
    report.check("SELinux is enforcing", selinux_enforcing(&vm));
    report.check("the tool installs from the second drive", install_from_drive(&vm, &certificate));
    report.check("a user runs it", user_runs_tool(&vm));
    if !report.has_failures() {
        report.check("it is back after a reboot", back_after_reboot(&mut vm));
    }
    report.check("SELinux denied nothing of drupe's", no_denials(&vm));
    drop(vm);
    let _ = fs::remove_file(&drive);
    report.finish()
}

fn selinux_enforcing(vm: &Vm) -> Result<()> {
    vm.ssh("timeout 300 systemctl is-system-running --wait")?;
    let mode = vm.ssh_ok("getenforce")?;
    let root = vm.ssh_ok("findmnt -no FSTYPE /")?;
    println!("     SELinux {mode}, / is {root}");
    ensure(mode == "Enforcing", || format!("SELinux is {mode}"))
}

fn install_from_drive(vm: &Vm, certificate: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "mkdir -p /etc/verity.d && cat > /etc/verity.d/drupe.crt <<'EOF'\n{certificate}\nEOF"
    ))?;
    // The drive is the image, byte for byte: the whole-disk device of the
    // only disk with a verity signature partition (the machine's own has
    // none). `drupe install` takes a file.
    let installed = vm.ssh_ok(
        "drive=/dev/$(lsblk -no PKNAME /dev/disk/by-partlabel/root-x86-64-verity-sig) && \
         dd if=$drive of=/var/tmp/strace.raw bs=1M status=none && \
         drupe install /var/tmp/strace.raw && rm /var/tmp/strace.raw",
    )?;
    println!("     {installed}");
    Ok(())
}

/// As in the ephemeral VM, but here `/app` is a directory of a composefs
/// root, the helper's setuid bit comes out of the ostree deployment and
/// SELinux has a say on every step.
fn user_runs_tool(vm: &Vm) -> Result<()> {
    let trace = vm.ssh_ok(&as_user(
        "/run/drupe/bin/strace -e trace=execve,openat /usr/bin/true 2>&1",
    ))?;
    for want in ["execve(\"/usr/bin/true\"", "\"/lib64/libc.so.6\"", "+++ exited with 0 +++"] {
        ensure(trace.contains(want), || {
            format!("no `{want}` in the trace:\n{trace}")
        })?;
    }
    let context = vm.ssh_ok(&as_user(&format!(
        "{ENTER} {TOOL} - /usr/bin/cat cat /proc/self/attr/current"
    )))?;
    let labels = vm.ssh_ok(&format!("ls -Zd /app {ENTER} /run/drupe/priv/strace/bin/strace"))?;
    println!("     runs as {}\n     {}", context.trim_matches('\0'), labels.replace('\n', "\n     "));
    Ok(())
}

fn back_after_reboot(vm: &mut Vm) -> Result<()> {
    vm.reboot(vmtest::BOOT_TIMEOUT)?;
    let state = vm.ssh("timeout 300 systemctl is-system-running --wait")?;
    let state = String::from_utf8_lossy(&state.stdout).trim().to_string();
    let unit = vm.ssh("systemctl is-active drupe.service")?;
    let unit = String::from_utf8_lossy(&unit.stdout).trim().to_string();
    if state != "running" || unit != "active" {
        let log = vm.ssh_ok("systemctl --failed --no-legend --plain; journalctl -b -u drupe.service --no-pager | tail -20")?;
        bail!("after the reboot the system is `{state}` and drupe.service `{unit}`:\n{log}");
    }
    vm.ssh_ok(&as_user("drupe run strace -V"))?;
    let list = vm.ssh_ok("drupe list")?;
    ensure(list.starts_with("strace\t"), || {
        format!("unexpected `drupe list` after the reboot:\n{list}")
    })
}

/// Nothing drupe did was denied (and so, in enforcing mode, blocked).
fn no_denials(vm: &Vm) -> Result<()> {
    let denials = vm.ssh_ok(
        "journalctl -b --no-pager -g 'avc:.*denied' 2>/dev/null \
         | grep -E 'drupe|strace|erofs|/app|verity' || true",
    )?;
    ensure(denials.is_empty(), || format!("AVC denials:\n{denials}"))
}
