//! Native Windows harness dispatch. Linux-only tiers continue in the builder.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{BootSource, Capability, DiskFormat, Resources, VmProvider},
};
use ilex_tooling_windows::gen2;
use std::{path::PathBuf, process::Command};

const TRIAL: &str = "security::firmware";

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
                if exact {
                    TRIAL == filter
                } else {
                    TRIAL.contains(&filter)
                },
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
        .env("ILEX_ROOT", &ws.root);
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
        let disk = crate::remote::gen2_disk(ws, &args, image)?;
        test.env("ILEX_GEN2_DISK", &disk)
            .env("ILEX_GEN2_SELECTION", serde_json::to_string(&selection)?)
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
    use std::{fs, time::Duration};
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
    let media = state.join("gen2").join(format!("media-{}", spec.run));
    fs::create_dir(&media)?;
    let parent = media.join("installed.vhdx");
    let result = (|| -> Result<()> {
        gen2::vhdx(&source, &parent, "qcow2")?;
        spec.boot = BootSource::Disk {
            path: parent.clone(),
            format: DiskFormat::Vhdx,
            writable: true,
        };
        spec.network = NetworkIntent::User { ssh: true };
        spec.ssh = Some(SshProvisioning {
            user: "root".into(),
            public_key: String::new(),
            recipe_hash: ilex_devkit::core::digest(b"ilex-gen2-v1;seed-host-key;seed-access-key"),
        });
        let mut vm = provider.create(spec)?;
        provider.start(&mut vm)?;
        eprintln!(
            "Gen2 VM {}. Enter the disposable fixture key ilex-gen2-recovery-410625 at its recovery prompt.",
            vm.name()
        );
        eprintln!(
            "Console control from this checkout uses ilex-vmctl --vm {}.",
            vm.purpose()
        );
        let work = (|| -> Result<()> {
            let _viewer = if args.gui {
                Some(
                    Command::new("vmconnect.exe")
                        .arg("localhost")
                        .arg(vm.name())
                        .spawn()?,
                )
            } else {
                None
            };
            vm.wait_ssh(None, Duration::from_secs(420))?;
            if !args.command.is_empty() {
                use std::io::Write;
                std::io::stdout().write_all(&vm.guest_exec(&args.command)?)?;
            } else if !args.gui {
                vm.interactive()?;
            } else {
                let mut viewer = _viewer.unwrap();
                while viewer.try_wait()?.is_none() && !ilex_tooling_windows::transport::cancelled()
                {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Ok(())
        })();
        provider.destroy_owned(&mut vm)?;
        work
    })();
    if result.is_ok() {
        fs::remove_file(parent)?;
        fs::remove_dir(media)?;
    } else {
        eprintln!("Gen2 media retained at {}", media.display());
    }
    result
}
