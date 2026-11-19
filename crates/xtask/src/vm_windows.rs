use anyhow::{Result, ensure};
use ilex_devkit::Workspace;
use ilex_tooling_windows::{
    qemu::{Config, Vm},
    session::Session,
    transport,
};
use std::{process::Stdio, thread, time::Duration};

pub fn run(ws: &Workspace, args: crate::vm::Args, resolved: crate::select::Resolved) -> Result<()> {
    if args.print_image {
        println!(
            "{}",
            crate::dev_image::reference(ws, &resolved.image, false)
        );
        return Ok(());
    }
    transport::install_cancel_handler()?;
    ws.ensure_state()?;
    ws.ensure_runtime()?;
    // T3 resolves/checks the actual image and returns the installed immutable
    // qcow2, authenticated manifest and ownership receipt to this checkout.
    let disk = if let Some(path) = std::env::var_os("ILEX_DISK").filter(|value| !value.is_empty()) {
        ilex_tooling_windows::qemu::import_disk(
            std::path::Path::new(&path),
            &ws.identity()?,
            &ws.state_dir(),
        )?
    } else {
        crate::remote::development_disk(ws, args.selection.clone(), args.image.clone())?
    };
    let mut vm = Vm::boot(Config {
        identity: ws.identity()?,
        runtime_dir: ws.runtime_dir(),
        state_dir: ws.state_dir().join("native-vm"),
        disk,
        purpose: args
            .purpose
            .clone()
            .unwrap_or_else(|| if args.gui { "gui" } else { "vm" }.into()),
        gui: args.gui,
        gl: args.gui && !args.no_gl,
        apps: args.apps_enabled(),
        memory_bytes: ilex_devkit::provider::memory_bytes(args.memory.as_deref().unwrap_or("8G"))?,
        vcpus: args.vcpus.unwrap_or(4),
        persist: args.persist,
    })?;
    eprintln!(
        "ilex: native WHPX VM {}; Right-Ctrl+G releases SDL grab",
        vm.info.name
    );
    eprintln!("{}", vm.info.ssh_command_line());
    vm.wait_ssh()?;
    ilex_tooling_windows::session::prepare(&vm.info)?;
    let _session = if args.gui {
        Some(Session::start(vm.info.clone())?)
    } else {
        None
    };
    let _resize = args
        .gui
        .then(|| ilex_tooling_windows::session::ResizeSession::start(vm.info.clone()));
    if !args.command.is_empty() || !args.gui {
        let mut ssh = if args.command.is_empty() {
            vm.info.interactive_ssh()
        } else {
            vm.info.ssh()
        };
        if !args.command.is_empty() {
            ssh.arg(
                args.command
                    .iter()
                    .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        let mut child = ssh
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "guest command failed ({status})");
                break;
            }
            if transport::cancelled() || vm.exited()? {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    } else {
        while !transport::cancelled() && !vm.exited()? {
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}
