//! Native Windows harness dispatch. Linux-only tiers continue in the builder.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{BootSource, Capability, DiskFormat, Resources, VmProvider},
};
use ilex_tooling_windows::gen2;
use std::{path::PathBuf, process::Command};

const TRIAL: &str = "security::firmware";
const TRANSITION: &str = "security::iso_transition";

pub fn run(ws: &Workspace, args: crate::security::Args) -> Result<()> {
    ensure!(
        args.accelerator.as_deref().unwrap_or("hyperv") == "hyperv",
        "Windows security requires the native Hyper-V accelerator"
    );
    let provider = gen2::Provider::new(ws.identity()?, &ws.ensure_state()?)?;
    if let Some(run) = args.cleanup_run {
        return provider.cleanup_run(&run);
    }
    let memory = args
        .memory
        .or_else(|| std::env::var("ILEX_SECURITY_MEMORY").ok())
        .unwrap_or_else(|| "6G".into());
    let request = gen2::spec(
        &ws.identity()?,
        "security-probe",
        resources(&memory)?,
        BootSource::BlankDisk {
            bytes: 32 << 30,
            format: DiskFormat::Vhdx,
        },
    )?;
    let caps = provider.probe(&request)?;
    println!("{}", serde_json::to_string_pretty(&caps)?);
    if args.require_qualified {
        caps.require(Capability::FirmwareEnforcement)?;
        caps.require(Capability::PersistentTpm)?;
    }
    if args.self_check {
        native(
            ws,
            crate::test::Args {
                selection: Default::default(),
                build: false,
                no_pull: false,
                filters: vec![TRIAL.into()],
                test_args: vec![],
            },
            &memory,
        )?;
    }
    Ok(())
}

fn resources(memory: &str) -> Result<Resources> {
    Ok(Resources {
        memory_bytes: ilex_devkit::provider::memory_bytes(memory)?,
        vcpus: std::env::var("ILEX_SECURITY_VCPUS")
            .ok()
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(2),
    })
}

pub fn partition(
    mut args: crate::test::Args,
) -> Result<(Option<crate::test::Args>, crate::test::Args)> {
    let exact = args.test_args.iter().any(|a| a == "--exact");
    let mut native = vec![];
    let mut legacy = vec![];
    for filter in args.filters.drain(..) {
        if filter.contains("security") {
            ensure!(
                [TRIAL, TRANSITION].iter().any(|trial| if exact {
                    *trial == filter
                } else {
                    trial.contains(&filter)
                }),
                "Windows security trial {filter:?} unavailable"
            );
            native.push(filter);
        } else {
            legacy.push(filter);
        }
    }
    let linux = (!legacy.is_empty()).then(|| crate::test::Args {
        selection: args.selection.clone(),
        build: args.build,
        no_pull: args.no_pull,
        filters: legacy,
        test_args: args.test_args.clone(),
    });
    args.filters = native;
    Ok((linux, args))
}

pub fn test(ws: &Workspace, args: crate::test::Args) -> Result<()> {
    let memory = std::env::var("ILEX_SECURITY_MEMORY").unwrap_or_else(|_| "6G".into());
    native(ws, args, &memory)
}

fn native(ws: &Workspace, args: crate::test::Args, memory: &str) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut test = Command::new(cargo);
    test.current_dir(&ws.root)
        .args(["test", "--package", "ilex-e2e", "--test", "e2e", "--"])
        .args(&args.filters)
        .args(&args.test_args)
        .env("ILEX_ROOT", &ws.root)
        .env("ILEX_XTASK", std::env::current_exe()?);
    if !args.test_args.iter().any(|a| a == "--list") {
        let iso = PathBuf::from(
            std::env::var_os("ILEX_ISO")
                .context("set ILEX_ISO to the exact signed installer ISO")?,
        );
        let declared = std::env::var("ILEX_ISO_SHA256").ok()
            .or_else(|| std::fs::read_to_string(iso.with_extension("iso.sha256")).ok()
                .and_then(|s| s.split_whitespace().next().map(str::to_owned)))
            .context("provide the installer asset SHA256 through ILEX_ISO_SHA256 or its .iso.sha256 file")?;
        ensure!(
            declared.len() == 64 && declared.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid installer SHA256"
        );
        let iso = std::path::absolute(iso)?;
        ensure!(iso.is_file(), "installer ISO does not exist");
        let image = std::env::var("ILEX_IMAGE").ok().filter(|s| !s.is_empty());
        let selection = args
            .selection
            .resolve(ws, image.as_deref(), None)?
            .selection
            .context("missing expected ilex selection")?;
        let resources = resources(memory)?;
        let identity = ws.identity()?;
        let provider = gen2::Provider::new(identity.clone(), &ws.ensure_state()?)?;
        let mut probe = gen2::spec(
            &identity,
            "security-probe",
            resources.clone(),
            BootSource::BlankDisk {
                bytes: 32 << 30,
                format: DiskFormat::Vhdx,
            },
        )?;
        probe.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
        provider.validate(&probe)?;
        let exact = args.test_args.iter().any(|arg| arg == "--exact");
        if args
            .filters
            .iter()
            .any(|f| if exact { f == TRIAL } else { TRIAL.contains(f) })
        {
            let disk = crate::remote::gen2_disk(ws, &args, image)?;
            test.env("ILEX_GEN2_DISK", disk);
        }
        test.env("ILEX_GEN2_SELECTION", serde_json::to_string(&selection)?)
            .env("ILEX_GEN2_RESOURCES", serde_json::to_string(&resources)?)
            .env("ILEX_ISO", iso)
            .env("ILEX_ISO_SHA256", declared);
    }
    cmd::run(&mut test)
}

/// Launch an ordinary qualified Gen2 guest. The same handle owns its disk and
/// vTPM from launch until cleanup. A console client is only an attachment.
pub fn vm(ws: &Workspace, args: crate::vm::Args, resolved: crate::select::Resolved) -> Result<()> {
    use ilex_devkit::core::{NetworkIntent, SshProvisioning, VmControl};
    use std::time::Duration;
    if args.print_image {
        println!("{}", crate::dev_image::reference(ws, &resolved.image, true));
        return Ok(());
    }
    ilex_tooling_windows::transport::install_cancel_handler()?;
    let state = ws.ensure_state()?;
    let identity = ws.identity()?;
    let provider = gen2::Provider::new(identity.clone(), &state)?;
    let mut resources = resources(args.memory.as_deref().unwrap_or("6G"))?;
    if let Some(vcpus) = args.vcpus {
        resources.vcpus = vcpus;
    }
    let mut spec = gen2::spec(
        &identity,
        args.purpose.as_deref().unwrap_or("secure"),
        resources,
        BootSource::BlankDisk {
            bytes: 32 << 30,
            format: DiskFormat::Vhdx,
        },
    )?;
    provider.validate(&spec)?;
    let source = crate::remote::gen2_disk(
        ws,
        &crate::test::Args {
            selection: args.selection,
            build: false,
            no_pull: false,
            filters: vec![],
            test_args: vec![],
        },
        args.image,
    )?;
    let receipt = source
        .parent()
        .and_then(std::path::Path::parent)
        .context("missing Gen2 disk collection")?
        .join("disk.windows.json");
    let artifact: ilex_devkit::core::DiskArtifact =
        serde_json::from_slice(&std::fs::read(receipt)?)?;
    ensure!(
        artifact.location.path == source,
        "Gen2 disk receipt does not match the delivery"
    );
    let parent = provider.installed_parent(&artifact)?;
    spec.boot = BootSource::Disk {
        path: parent,
        format: DiskFormat::Vhdx,
        writable: true,
    };
    spec.network = NetworkIntent::User { ssh: true };
    spec.ssh = Some(SshProvisioning {
        user: "root".into(),
        public_key: String::new(),
        recipe_hash: artifact.provisioning_hash,
    });
    let mut vm = provider.create(spec)?;
    let work = (|| -> Result<()> {
        let recovery = vm.conceal_console()?;
        provider.start(&mut vm)?;
        eprintln!(
            "Gen2 VM {}. The disposable console response is documented in windows-gen2.md.",
            vm.name()
        );
        eprintln!(
            "Console control from this checkout uses ilex-vmctl --vm {}.",
            vm.purpose()
        );
        let viewer = if args.gui {
            Some(
                Command::new("vmconnect.exe")
                    .arg("localhost")
                    .arg(vm.name())
                    .spawn()?,
            )
        } else {
            None
        };
        let boot = vm.wait_ssh(None, Duration::from_secs(420))?;
        let acknowledged = vm.acknowledge_sensitive_input(
            &recovery,
            &["cat".into(), "/var/lib/ilex-gen2/recovery-ok".into()],
            format!("{boot}\n").as_bytes(),
        )?;
        if acknowledged {
            let pointer = vm.capture().and_then(|frame| {
                vm.qualify_pointer(&frame, frame.width / 3, frame.height * 2 / 3)
            });
            if let Err(error) = pointer {
                eprintln!("Gen2 pointer remains unqualified: {error:#}");
            }
        } else {
            eprintln!("Gen2 capture remains withheld for newer private input");
        }
        if let Err(error) = vm.qualify_session_keyboard() {
            eprintln!("Gen2 session keyboard remains unqualified: {error:#}");
        }
        if !args.command.is_empty() {
            use std::io::Write;
            std::io::stdout().write_all(&vm.guest_exec(&args.command)?)?;
        } else if let Some(mut viewer) = viewer {
            while viewer.try_wait()?.is_none() && !ilex_tooling_windows::transport::cancelled() {
                std::thread::sleep(Duration::from_millis(250));
            }
        } else {
            vm.interactive()?;
        }
        Ok(())
    })();
    let cleanup = provider.destroy_owned(&mut vm);
    match (work, cleanup) {
        (Err(work), Err(teardown)) => {
            Err(work.context(format!("Gen2 teardown also failed: {teardown:#}")))
        }
        (work, cleanup) => work.and(cleanup),
    }
}
