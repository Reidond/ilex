use super::{Command, Job, Operation, source};
use anyhow::{Context, Result, ensure};
use ilex_devkit::core::{TransferChunk, TransferOperation};
use ilex_devkit::{Workspace, core::digest};
use ilex_tooling_windows::transport::{self, Transport};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ENVIRONMENT: &[&str] = &[
    "ILEX_IMAGE",
    "ILEX_UPGRADE_FROM",
    "ILEX_SHELLS",
    "ILEX_MACHINE",
    "ILEX_UPGRADE_PREPARE",
    "ILEX_SIGPOLICY_LEGACY",
    "ILEX_TOOL_REBUILD",
    "ILEX_ISO_MACHINE",
    "ILEX_ISO_EXPECT",
    "ILEX_ISO_TARGET",
    "ILEX_ISO_DIGEST",
    "ILEX_ISO_INSTALL_TIMEOUT",
    "ILEX_VM_SLOTS",
    "ILEX_KACHE_IMPORT",
];
type Rpc<'a> = TransferOperation<'a, Job>;

pub(super) fn job_uuid() -> Result<String> {
    // Windows supplies the random UUID; never derive a run ID from PID/time alone.
    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoCreateGuid(value: *mut Guid) -> i32;
    }
    let mut value = Guid {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };
    ensure!(
        unsafe { CoCreateGuid(&mut value) } == 0,
        "allocating job UUID"
    );
    let tail: String = value.data4[2..]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{tail}",
        value.data1, value.data2, value.data3, value.data4[0], value.data4[1]
    ))
}

fn state(ws: &Workspace) -> Result<PathBuf> {
    let root = ws.ensure_state()?.join("remote");
    fs::create_dir_all(root.join("jobs"))?;
    Ok(root)
}

fn client_lock(root: &Path) -> Result<fs::File> {
    let guard = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("client.lock"))?;
    guard
        .try_lock()
        .context("another checkout client is active")?;
    Ok(guard)
}

fn delivered_disk(ws: &Workspace, directory: &Path, request: &Job) -> Result<PathBuf> {
    use ilex_tooling_windows::received::Directory;
    let control = Directory::open(directory)?;
    let collection: Value = control_json(&control, "collection.json")?;
    let identity = ws.identity()?;
    let job = directory.file_name().context("missing local job ID")?;
    ensure!(
        collection["schema"] == 1
            && collection["owner"] == identity.owner
            && collection["job"] == request.job
            && collection["source_digest"] == request.snapshot.source.digest
            && collection["exit_code"] == 0,
        "invalid disk collection identity"
    );
    let received = PathBuf::from(
        collection["received"]
            .as_str()
            .context("missing disk collection path")?,
    );
    ensure!(
        received.parent() == Some(ws.state_dir().join("remote/received").join(job).as_path()),
        "disk collection escaped its job"
    );
    let attempt = Directory::open(&received)?;
    let artifact: ilex_devkit::core::DiskArtifact =
        serde_json::from_value(control_json(&attempt, "disk.windows.json")?)?;
    let relative = artifact
        .location
        .path
        .strip_prefix(&received)?
        .to_str()
        .context("invalid disk path")?
        .replace('\\', "/");
    let _held = attempt.read(&relative)?;
    if matches!(&request.command, Command::Disk(a) if a.gen2.is_some()) {
        ensure!(
            artifact.provisioning_hash == digest(b"ilex-gen2-v1;seed-host-key;seed-access-key")
                && artifact.location.executor_id == format!("windows-owner:{}", identity.owner)
                && artifact.location.sha256 == hash_file(&artifact.location.path)?,
            "delivered Gen2 disk ownership, provisioning, or hash mismatch"
        );
        return Ok(artifact.location.path);
    }
    ilex_tooling_windows::qemu::cache_delivered_disk(artifact, &identity, &ws.state_dir())
}

pub fn execute(ws: &Workspace, mut command: Command) -> Result<Option<PathBuf>> {
    let root = state(ws)?;
    let _checkout_lock = client_lock(&root)?;
    validate(ws, &command)?;
    let started = Instant::now();
    let disk_out = if let Command::Disk(a) = &mut command {
        a.out.take()
    } else {
        None
    }
    .map(std::path::absolute)
    .transpose()?;
    let iso_out = if let Command::Iso(a) = &mut command {
        a.output_dir.take()
    } else {
        None
    }
    .map(std::path::absolute)
    .transpose()?;
    let archive_out = if let Command::Export { output, .. } = &mut command {
        output.take()
    } else {
        None
    }
    .map(std::path::absolute)
    .transpose()?;
    let artifacts_out = std::env::var_os("ILEX_ARTIFACTS")
        .filter(|_| !matches!(&command, Command::Oemdrv { .. }))
        .map(std::path::absolute)
        .transpose()?;
    let _outputs = claim_outputs(
        ws,
        disk_out.as_deref(),
        archive_out.as_deref(),
        iso_out.as_deref(),
        artifacts_out.as_deref(),
    )?;
    let mut staged = source::stage(ws)?;
    let mut inputs = BTreeMap::new();
    if let Command::ImportKache { archive, .. } = &mut command {
        let archive = archive.take().context("missing local kache archive")?;
        inputs.insert("kache".into(), staged.file(&archive)?);
    }
    if matches!(&command, Command::Test(a) if a.filters.iter().any(|f| f.contains("iso")))
        && let Some(iso) = std::env::var_os("ILEX_ISO").filter(|s| !s.is_empty())
    {
        let iso = PathBuf::from(iso);
        inputs.insert("iso".into(), staged.file(&iso)?);
        inputs.insert(
            "catalogue".into(),
            staged.file(&iso.with_file_name("compositions.json"))?,
        );
    }
    let environment = ENVIRONMENT
        .iter()
        .filter(|&&key| key != "ILEX_KACHE_IMPORT" || matches!(&command, Command::Build(_)))
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect();
    let job = Job {
        schema: 1,
        job: job_uuid()?,
        snapshot: staged.snapshot.clone(),
        command,
        environment,
        inputs,
    };
    let snapshot_seconds = started.elapsed().as_secs_f64();
    let job_dir = root.join("jobs").join(&job.job);
    fs::create_dir_all(&job_dir)?;
    fs::write(
        job_dir.join("request.json"),
        serde_json::to_vec_pretty(&job)?,
    )?;
    fs::write(
        job_dir.join("destinations.json"),
        serde_json::to_vec(
            &json!({"disk":disk_out,"iso":iso_out,"archive":archive_out,"artifacts":artifacts_out}),
        )?,
    )?;
    // Save before submit: a lost reply never loses the idempotent job identity.
    fs::write(root.join("last-job"), &job.job)?;
    let builder_started = Instant::now();
    let transport = Transport::connect()?;
    let builder_seconds = builder_started.elapsed().as_secs_f64();
    transport::install_cancel_handler()?;
    fs::write(
        job_dir.join("endpoint.json"),
        serde_json::to_vec(&transport.endpoint)?,
    )?;
    let upload_started = Instant::now();
    let keys: Vec<_> = staged.chunks.keys().cloned().collect();
    let missing = transport.rpc(Rpc::Missing { chunks: &keys }, &[])?.0;
    let mut sent = 0u64;
    let mut batch = Vec::new();
    let mut descriptors = Vec::new();
    for key in missing["missing"]
        .as_array()
        .context("missing chunk list")?
    {
        ensure!(
            !transport::cancelled(),
            "snapshot transfer cancelled; no remote job started"
        );
        let key = key.as_str().context("invalid missing chunk key")?;
        let bytes = fs::read(
            staged
                .chunks
                .get(key)
                .context("builder requested an unknown source chunk")?,
        )?;
        if batch.len() + bytes.len() > ilex_devkit::core::CHUNK_BYTES && !batch.is_empty() {
            transport.rpc(
                Rpc::PutMany {
                    chunks: &descriptors,
                },
                &batch,
            )?;
            batch.clear();
            descriptors.clear();
        }
        descriptors.push(TransferChunk {
            sha256: key,
            bytes: bytes.len(),
        });
        batch.extend_from_slice(&bytes);
        sent += bytes.len() as u64;
    }
    if !batch.is_empty() {
        transport.rpc(
            Rpc::PutMany {
                chunks: &descriptors,
            },
            &batch,
        )?;
    }
    eprintln!(
        "ilex: snapshot {} ({} source bytes; {sent} bytes uploaded) owner {}",
        job.snapshot.source.digest, staged.bytes, job.snapshot.identity.owner
    );
    let interactive = matches!(&job.command, Command::Vm(args) if args.command.is_empty());
    let heartbeat =
        interactive.then(|| transport.terminal_heartbeat(&job.snapshot.identity.owner, &job.job));
    let registered = transport.register(&job);
    if interactive && transport::cancelled() {
        drop(heartbeat);
        // A lost submit reply can still leave a durable registration.
        return wait(ws, &transport, &job, &job_dir, true).context("interactive submit cancelled");
    }
    let reply = registered?;
    eprintln!("ilex: remote job {} at {}", job.job, reply["root"]);
    fs::write(
        job_dir.join("transfer.json"),
        serde_json::to_vec_pretty(&json!({
            "schema":1,"source_bytes":staged.bytes,"uploaded_bytes":sent,"chunks":staged.chunks.len(),
        "stage_seconds":started.elapsed().as_secs_f64(),"snapshot_seconds":snapshot_seconds,
        "builder_seconds":builder_seconds,"upload_seconds":upload_started.elapsed().as_secs_f64(),
        "snapshot":job.snapshot.source.digest,"builder_root":reply["root"],
            "started_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        }))?,
    )?;
    if interactive {
        let terminal = transport.terminal(&job.snapshot.identity.owner, &job.job);
        drop(heartbeat);
        let result = wait(ws, &transport, &job, &job_dir, terminal.is_err());
        if transport::cancelled() {
            return result;
        }
        return terminal.and(result);
    }
    wait(ws, &transport, &job, &job_dir, false)
}

pub fn run(ws: &Workspace, operation: Operation) -> Result<()> {
    match operation {
        Operation::Oemdrv { source } => {
            let source = ilex_tooling_windows::received::Directory::open(&source)?;
            let read = |name| -> Result<String> {
                let mut text = String::new();
                source
                    .read(name)?
                    .file
                    .take(65537)
                    .read_to_string(&mut text)?;
                ensure!(
                    !text.is_empty() && text.len() <= 65536,
                    "invalid OEMDRV source size"
                );
                Ok(text)
            };
            let artifact = execute(
                ws,
                Command::Oemdrv {
                    kickstart: read("ks.cfg")?,
                    public_key: read("recovery.pub")?,
                },
            )?
            .context("missing OEMDRV image")?;
            println!("{}", artifact.display());
            Ok(())
        }
        Operation::Snapshot => {
            let root = state(ws)?;
            let _guard = client_lock(&root)?;
            let staged = source::stage(ws)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"schema":1,"snapshot":staged.snapshot,"source_bytes":staged.bytes,"chunks":staged.chunks.keys().collect::<Vec<_>>(),"cache":staged.cache})
                )?
            );
            Ok(())
        }
        Operation::SelfCheck {
            hold_seconds,
            allocation_image,
            fingerprint_image,
        } => execute(
            ws,
            Command::SelfCheck {
                hold_seconds,
                allocation_image,
                fingerprint_image,
            },
        )
        .map(|_| ()),
        Operation::SelfCheckReceiver => super::receive::self_check(ws),
        Operation::Export { image, out } => execute(
            ws,
            Command::Export {
                image,
                output: Some(out),
            },
        )
        .map(|_| ()),
        Operation::Cancel { job } => {
            // Sending a cancellation must not wait for the client's collection
            // lock. The live client collects; an absent client can resume later.
            let root = state(ws)?;
            let id = match job {
                Some(id) => id,
                None => fs::read_to_string(root.join("last-job"))?,
            };
            ensure!(
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
                "invalid job ID"
            );
            let control =
                ilex_tooling_windows::received::Directory::open(&root.join("jobs").join(&id))?;
            let mut bytes = Vec::new();
            control
                .read("request.json")?
                .file
                .take(32 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 32 * 1024 * 1024, "local request size limit");
            let job: Job = serde_json::from_slice(&bytes)?;
            ensure!(job.job == id, "local job identity mismatch");
            ws.identity()?.authorize(&job.snapshot.identity.owner)?;
            let transport = Transport::existing(control_json(&control, "endpoint.json")?)?;
            let (reply, _) = transport.rpc(
                Rpc::Cancel {
                    owner: &job.snapshot.identity.owner,
                    job: &job.job,
                },
                &[],
            )?;
            match reply["cancel"].as_str() {
                Some("confirmed") => eprintln!(
                    "ilex: cancellation confirmed for {}. The running client collects evidence, or use remote resume",
                    job.job
                ),
                Some("finished") => eprintln!("ilex: job {} already finished", job.job),
                Some("pending") => anyhow::bail!(
                    "cancel pending for {} ({})",
                    job.job,
                    reply["detail"]
                        .as_str()
                        .unwrap_or("waiting for the worker to stop")
                ),
                _ => anyhow::bail!("invalid builder cancellation reply"),
            }
            Ok(())
        }
        Operation::Resume { job } => {
            let root = state(ws)?;
            let _guard = client_lock(&root).context("job is not resumable now")?;
            let id = match job {
                Some(id) => id,
                None => fs::read_to_string(root.join("last-job"))?,
            };
            ensure!(
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
                "invalid job ID"
            );
            let dir = root.join("jobs").join(&id);
            let control = ilex_tooling_windows::received::Directory::open(&dir)?;
            let job: Job = control_json(&control, "request.json")?;
            ensure!(job.job == id, "local job identity mismatch");
            ws.identity()?.authorize(&job.snapshot.identity.owner)?;
            if matches!(&job.command, Command::Disk(a) if a.development || a.gen2.is_some()) {
                match control.read("collection.json") {
                    Ok(receipt) => {
                        drop(receipt);
                        let collection: Value = control_json(&control, "collection.json")?;
                        if collection["exit_code"] == 0 {
                            let disk = delivered_disk(ws, &dir, &job)?;
                            println!("{}", disk.display());
                            return Ok(());
                        }
                    }
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            let destinations: Value = control_json(&control, "destinations.json")?;
            let _outputs = claim_outputs(
                ws,
                destinations["disk"].as_str().map(Path::new),
                destinations["archive"].as_str().map(Path::new),
                destinations["iso"].as_str().map(Path::new),
                destinations["artifacts"].as_str().map(Path::new),
            )?;
            let transport = Transport::connect()?;
            if matches!(&job.command, Command::Vm(args) if args.command.is_empty()) {
                transport.resume_terminal(&job.snapshot.identity.owner, &job.job)?;
            }
            transport::install_cancel_handler()?;
            wait(ws, &transport, &job, &dir, false).map(|_| ())
        }
    }
}

fn validate(ws: &Workspace, command: &Command) -> Result<()> {
    if let Command::Vm(args) = command {
        use std::io::IsTerminal;
        args.resolve(ws)?;
        ensure!(!args.persist, "--persist only applies to --disk/--gui");
        ensure!(
            !args.command.is_empty() || std::io::stdin().is_terminal(),
            "ephemeral VM shell needs a terminal or a command after --"
        );
    }
    if let Command::Iso(args) = command {
        // CI is deliberately absent from the remote environment. Apply the
        // existing local-only testing-tag rule before crossing that boundary.
        ilex_devkit::installer::table(&ws.root, args.testing_tag.as_deref())?;
    }
    if let Command::Test(args) = command {
        let native: Vec<_> = args
            .filters
            .iter()
            .filter(|f| f.contains("security") || f.contains("homed"))
            .collect();
        ensure!(
            native.is_empty(),
            "native Windows tiers require the T5 harness: {native:?}; no filters were silently dropped"
        );
        ensure!(
            !args
                .test_args
                .iter()
                .any(|a| a == "--skip" || a.starts_with("--skip="))
                || !args.filters.is_empty(),
            "supply explicit legacy filters with --skip"
        );
    }
    if let Command::Build(args) = command {
        ilex_devkit::ownership::validate_build_args(&args.podman_args)?;
    }
    Ok(())
}

fn wait(
    ws: &Workspace,
    transport: &Transport,
    job: &Job,
    dir: &Path,
    cancel: bool,
) -> Result<Option<PathBuf>> {
    use super::receive;
    use ilex_tooling_windows::received::{self, Directory};
    let owner = &job.snapshot.identity.owner;
    ensure!(
        dir.file_name().and_then(|s| s.to_str()) == Some(job.job.as_str()),
        "job/control identity mismatch"
    );
    let control = Directory::open(dir)?;
    let received = Directory::open(&ws.state_dir().join("remote/received").join(&job.job))?;
    let logs = received.child("logs")?;
    let mut cancelled = false;
    let mut offset = [0u64; 2];
    for (i, name) in ["stdout", "stderr"].iter().enumerate() {
        offset[i] = logs.append(&format!("{name}.log"))?.file.metadata()?.len();
        ensure!(offset[i] <= receive::MAX_LOG, "saved log size limit");
    }
    let paths = matches!(
        job.command,
        Command::Disk(_) | Command::Tool(_) | Command::Export { .. }
    ) || matches!(&job.command, Command::Iso(a) if !a.check_registry && !a.due);
    let mut until = Instant::now() + transport::JOB_WAIT;
    let result = loop {
        ensure!(
            Instant::now() < until,
            "job {} did not publish within the wait limit. Use cargo xtask remote resume --job {}",
            job.job,
            job.job
        );
        if (cancel || transport::cancelled()) && !cancelled {
            let (reply, _) = transport.rpc(
                Rpc::Cancel {
                    owner,
                    job: &job.job,
                },
                &[],
            )?;
            cancelled = true;
            let status = match reply["cancel"].as_str() {
                Some("confirmed") => "cancellation confirmed",
                Some("finished") => "already finished",
                Some("pending") => "cancel pending",
                _ => "cancellation status unknown",
            };
            eprintln!("ilex: job {} {status}", job.job);
        }
        let status = transport.rpc(Rpc::Status { owner, job: &job.job }, &[])
            .with_context(|| format!("job {} retained; reconnect with `cargo xtask remote resume --job {}`; logs at {}", job.job, job.job, logs.path.display()))?.0;
        if matches!(
            status["state"]["state"].as_str(),
            Some("collecting" | "recovering")
        ) {
            until = until.min(Instant::now() + Duration::from_secs(6 * 60));
        }
        for (i, name) in ["stdout", "stderr"].iter().enumerate() {
            let end = status["logs"][*name]
                .as_u64()
                .context("invalid remote log size")?;
            ensure!(
                end <= receive::MAX_LOG && end >= offset[i],
                "remote log size/truncation limit"
            );
            while offset[i] < end {
                let (reply, bytes) = transport.rpc(
                    Rpc::Fetch {
                        owner,
                        job: &job.job,
                        path: &format!("job/{name}.log"),
                        offset: offset[i],
                    },
                    &[],
                )?;
                let count = receive::append_log(
                    &logs,
                    &format!("{name}.log"),
                    offset[i],
                    end,
                    reply["sha256"].as_str().context("missing log checksum")?,
                    &bytes,
                )?;
                if i == 0 && !paths {
                    std::io::stdout().write_all(&bytes[..count])?;
                } else {
                    std::io::stderr().write_all(&bytes[..count])?;
                }
                offset[i] += count as u64;
            }
        }
        if !status["result"].is_null() {
            break status["result"].clone();
        }
        thread::sleep(Duration::from_secs(2));
    };
    ensure!(
        result["schema"] == 1
            && result["owner"] == *owner
            && result["job"] == job.job
            && result["source_digest"] == job.snapshot.source.digest,
        "remote result identity mismatch"
    );
    let code = result["exit_code"]
        .as_i64()
        .context("missing remote exit code")?;
    ensure!((-127..=255).contains(&code), "invalid remote exit code");
    // Validate the complete untrusted manifest before writing any artifact.
    let manifest = receive::manifest(&result["artifacts"])?;
    if matches!(job.command, Command::Oemdrv { .. }) {
        receive::validate_oemdrv(&manifest, code == 0)?;
    }
    let result_bytes = serde_json::to_vec_pretty(&result)?;
    ensure!(
        result_bytes.len() <= 32 * 1024 * 1024,
        "remote result size limit"
    );
    let attempt = received.child(&job_uuid()?)?;
    attempt.write_new("result.json", &result_bytes)?;
    let destinations: Value = control_json(&control, "destinations.json")?;
    let primary_name = {
        let mut stdout = Vec::new();
        logs.read("stdout.log")?.file.read_to_end(&mut stdout)?;
        String::from_utf8_lossy(&stdout)
            .lines()
            .last()
            .map(|line| line.rsplit('/').next().unwrap_or(line).to_owned())
    };
    let delivered = receive::collect(&attempt, &result["artifacts"], |path, offset| {
        let (reply, bytes) = transport.rpc(
            Rpc::Fetch {
                owner,
                job: &job.job,
                path,
                offset,
            },
            &[],
        )?;
        ensure!(
            reply["sha256"] == digest(&bytes),
            "artifact chunk checksum mismatch"
        );
        Ok(bytes)
    })?;
    // Only locally authored destinations can request an export. Directory
    // exports receive a fresh per-job directory; remote names never address
    // checkout state or control metadata, even for a logical job/* artifact.
    for (name, destination) in &delivered {
        if name.starts_with("exports/")
            && primary_name
                .as_ref()
                .is_some_and(|base| name == &format!("exports/{base}"))
            && let Some(out) = destinations["archive"].as_str()
        {
            copy_atomic(destination, Path::new(out))?;
        }
        for prefix in ["artifacts", "iso"] {
            if let Some(tail) = name.strip_prefix(&format!("{prefix}/"))
                && let Some(root) = destinations[prefix].as_str()
            {
                let root = Directory::open(Path::new(root))?.child(&job.job)?;
                copy_atomic(destination, &root.path.join(tail))?;
            }
        }
        if name.starts_with("disks/")
            && primary_name
                .as_ref()
                .is_some_and(|base| name == &format!("disks/{base}"))
            && matches!(
                destination.extension().and_then(|e| e.to_str()),
                Some("qcow2" | "raw")
            )
            && let Some(out) = destinations["disk"].as_str()
        {
            copy_atomic(destination, Path::new(out))?;
        }
        eprintln!("ilex: artifact {}", destination.display());
    }
    let mut map = control.create(&format!("delivered-{}.partial", job_uuid()?))?;
    map.file
        .write_all(&serde_json::to_vec_pretty(&delivered)?)?;
    map.file.sync_all()?;
    received::rename(&map, "delivered.json")?;
    if matches!(job.command, Command::Disk(_))
        && let Some(base) = primary_name.as_ref()
        && let Some(disk) = delivered.get(&format!("disks/{base}"))
        && let Some(metadata) = delivered.get(&format!("disks/{base}.disk.json"))
    {
        let relative = metadata
            .strip_prefix(&attempt.path)?
            .to_str()
            .context("disk metadata path")?
            .replace('\\', "/");
        let mut bytes = Vec::new();
        attempt
            .read(&relative)?
            .file
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 32 * 1024 * 1024, "disk metadata size limit");
        let mut artifact: ilex_devkit::core::DiskArtifact = serde_json::from_slice(&bytes)?;
        ilex_devkit::core::check_version(artifact.schema)?;
        ensure!(
            artifact.location.sha256 == hash_file(disk)?,
            "installed disk metadata checksum mismatch"
        );
        artifact.location.path = disk.clone();
        artifact.location.executor_id = format!("windows-owner:{}", owner);
        artifact.allocated_bytes = transport::allocated_bytes(disk)?;
        attempt.write_new("disk.windows.json", &serde_json::to_vec_pretty(&artifact)?)?;
    }
    eprintln!("ilex: job {} evidence: {}", job.job, attempt.path.display());
    if let Some(error) = result["evidence_error"].as_str() {
        eprintln!("ilex: evidence collection ended early: {error}");
    }
    let mut receipt = control.create(&format!("collection-{}.partial", job_uuid()?))?;
    receipt.file.write_all(&serde_json::to_vec(&json!({"schema":1,"job":job.job,"owner":owner,"source_digest":job.snapshot.source.digest,"exit_code":code,"received":attempt.path}))?)?;
    receipt.file.sync_all()?;
    received::rename(&receipt, "collection.json")?;
    // A bound create handle denies sharing until dropped, including reopening
    // the just-published receipt during native disk promotion below.
    drop(receipt);
    if code != 0 {
        let code = if code < 0 { 128 - code } else { code };
        let message = if let Some(reason) = result["terminal_error"].as_str() {
            format!("interactive job {} is not resumable ({reason})", job.job)
        } else {
            format!("remote job {} exited {code}", job.job)
        };
        return Err(super::Failure {
            code: u8::try_from(code).unwrap_or(1),
            message: format!(
                "{message}. Artifacts retrieved to {}",
                attempt.path.display()
            ),
        }
        .into());
    }
    if matches!(&job.command, Command::Disk(a) if a.development || a.gen2.is_some()) {
        let disk = delivered_disk(ws, dir, job)?;
        println!("{}", disk.display());
        return Ok(Some(disk));
    }
    if matches!(&job.command, Command::Oemdrv { .. }) {
        let artifact = delivered
            .get("artifacts/answers.raw")
            .context("OEMDRV image was not delivered")?;
        ensure!(
            artifact.metadata()?.len() == ilex_tooling_windows::transport::OEMDRV_BYTES as u64,
            "incorrect OEMDRV image size"
        );
        return Ok(Some(artifact.clone()));
    }
    if paths && let Some(basename) = primary_name {
        received::relative(&basename)?;
        let prefix = match job.command {
            Command::Disk(_) => "disks",
            Command::Iso(_) => "iso",
            Command::Export { .. } => "exports",
            _ => "tools/images",
        };
        let location = match job.command {
            Command::Disk(_) => destinations["disk"].as_str().map(PathBuf::from),
            Command::Export { .. } => destinations["archive"].as_str().map(PathBuf::from),
            Command::Iso(_) => destinations["iso"]
                .as_str()
                .map(|p| Path::new(p).join(&job.job).join(&basename)),
            _ => None,
        }
        .or_else(|| delivered.get(&format!("{prefix}/{basename}")).cloned())
        .context("primary output was not delivered")?;
        println!("{}", location.display());
    }
    Ok(None)
}

struct OutputClaims {
    _files: Vec<fs::File>,
    _directories: Vec<ilex_tooling_windows::received::Directory>,
}
fn claim_outputs(
    ws: &Workspace,
    disk: Option<&Path>,
    archive: Option<&Path>,
    iso: Option<&Path>,
    artifacts: Option<&Path>,
) -> Result<OutputClaims> {
    let mut guards = Vec::new();
    let mut directories = Vec::new();
    for path in [disk, archive].into_iter().flatten() {
        let directory = ilex_tooling_windows::received::Directory::open(
            path.parent().context("output parent missing")?,
        )?;
        protect_output(
            ws,
            &directory
                .path
                .join(path.file_name().context("output filename missing")?),
        )?;
        directories.push(directory);
        guards.push(claim_output_file(ws, path)?);
    }
    for directory in [iso, artifacts].into_iter().flatten() {
        let guard = ilex_tooling_windows::received::Directory::open(directory)?;
        protect_output(ws, &guard.path)?;
        reject_reparse(directory)?;
        reject_reparse(&directory.join(".ilex-owner.json"))?;
        ilex_devkit::ownership::ensure_directory(directory, &ws.identity()?)?;
        directories.push(guard);
    }
    Ok(OutputClaims {
        _files: guards,
        _directories: directories,
    })
}

pub(super) fn protect_output(ws: &Workspace, path: &Path) -> Result<()> {
    let parent = path.parent().context("output has no parent")?;
    let canonical = if path.is_dir() {
        fs::canonicalize(path)?
    } else {
        fs::canonicalize(parent)?.join(path.file_name().context("output filename")?)
    };
    let mut protected = vec![ws.ensure_state()?];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        protected.push(PathBuf::from(local).join("ilex"));
    }
    let candidate = canonical.to_string_lossy().to_ascii_lowercase();
    for root in protected {
        let root = fs::canonicalize(root)?
            .to_string_lossy()
            .to_ascii_lowercase();
        ensure!(
            candidate != root && !candidate.starts_with(&(root + "\\")),
            "explicit output cannot address tooling state/control paths"
        );
    }
    Ok(())
}

fn claim_output_file(ws: &Workspace, path: &Path) -> Result<fs::File> {
    reject_reparse(path)?;
    let name = path
        .file_name()
        .context("output filename missing")?
        .to_string_lossy();
    ilex_tooling_windows::received::relative(&name)?;
    let receipt = path.with_file_name(format!("{name}.ilex-owner"));
    let lock = path.with_file_name(format!("{name}.ilex-output.lock"));
    reject_reparse(&receipt)?;
    reject_reparse(&lock)?;
    let guard = ilex_devkit::ownership::lock(&lock)?;
    ilex_devkit::ownership::claim_file(ws, path)?;
    Ok(guard)
}

fn reject_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => ensure!(
                meta.file_attributes() & 0x400 == 0,
                "reparse point in output path: {}",
                ancestor.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn hash_file(path: &Path) -> Result<String> {
    use ilex_tooling_windows::received::{self, Directory};
    let parent = Directory::open(path.parent().context("artifact parent missing")?)?;
    let mut input = parent.read(
        path.file_name()
            .and_then(|n| n.to_str())
            .context("artifact name")?,
    )?;
    received::sha256(&mut input.file)
}
fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    use ilex_tooling_windows::received::{self, Directory};
    let parent = Directory::open(source.parent().context("artifact parent missing")?)?;
    let mut source = parent.read(
        source
            .file_name()
            .and_then(|n| n.to_str())
            .context("artifact name")?,
    )?;
    let hash = received::sha256(&mut source.file)?;
    let destination_parent = Directory::open(
        destination
            .parent()
            .context("explicit output has no parent")?,
    )?;
    let name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .context("output name")?;
    received::relative(name)?;
    match destination_parent.read(name) {
        Ok(mut old) => {
            if received::sha256(&mut old.file)? == hash {
                return Ok(());
            }
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    ensure!(
        transport::available_space(destination)?
            >= source.file.metadata()?.len() + 30 * 1024 * 1024 * 1024,
        "explicit output requires its size plus 30 GiB free"
    );
    let mut output = destination_parent.create(&format!("ilex-output-{}.partial", job_uuid()?))?;
    use std::io::{Seek, SeekFrom};
    source.file.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut source.file, &mut output.file)?;
    output.file.sync_all()?;
    ensure!(
        received::sha256(&mut output.file)? == hash,
        "output copy checksum mismatch"
    );
    received::rename(&output, name)?;
    Ok(())
}

fn control_json<T: serde::de::DeserializeOwned>(
    directory: &ilex_tooling_windows::received::Directory,
    name: &str,
) -> Result<T> {
    let mut bytes = Vec::new();
    directory
        .read(name)?
        .file
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 32 * 1024 * 1024,
        "local control metadata size limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
