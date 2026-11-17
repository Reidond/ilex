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
    path::{Component, Path, PathBuf},
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
];
type Rpc<'a> = TransferOperation<'a, Job>;

fn job_uuid() -> Result<String> {
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

pub fn execute(ws: &Workspace, mut command: Command) -> Result<()> {
    let root = state(ws)?;
    let _checkout_lock = ilex_devkit::ownership::lock(&root.join("client.lock"))?;
    validate(&command)?;
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
    let mut staged = source::stage(ws)?;
    let mut inputs = BTreeMap::new();
    if matches!(command, Command::Test(_))
        && let Some(iso) = std::env::var_os("ILEX_ISO").filter(|s| !s.is_empty())
    {
        let iso = PathBuf::from(iso);
        inputs.insert("iso".into(), staged.file(&iso)?);
        inputs.insert(
            "catalogue".into(),
            staged.file(&iso.with_file_name("compositions.json"))?,
        );
    }
    let job = Job {
        schema: 1,
        job: job_uuid()?,
        snapshot: staged.snapshot.clone(),
        command,
        environment: ENVIRONMENT
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect(),
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
            &json!({"disk":disk_out,"iso":iso_out,"archive":archive_out,"artifacts":std::env::var_os("ILEX_ARTIFACTS").map(PathBuf::from)}),
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
    let reply = transport.register(&job)?;
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
    wait(ws, &transport, &job, &job_dir, false)
}

pub fn run(ws: &Workspace, operation: Operation) -> Result<()> {
    let cancel = matches!(operation, Operation::Cancel { .. });
    match operation {
        Operation::Snapshot => {
            let root = state(ws)?;
            let _guard = ilex_devkit::ownership::lock(&root.join("client.lock"))?;
            let staged = source::stage(ws)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"schema":1,"snapshot":staged.snapshot,"source_bytes":staged.bytes,"chunks":staged.chunks.keys().collect::<Vec<_>>(),"cache":staged.cache})
                )?
            );
            Ok(())
        }
        Operation::SelfCheck { hold_seconds } => execute(ws, Command::SelfCheck { hold_seconds }),
        Operation::Export { image, out } => execute(
            ws,
            Command::Export {
                image,
                output: Some(out),
            },
        ),
        Operation::Resume { job } | Operation::Cancel { job } => {
            let root = state(ws)?;
            let _guard = ilex_devkit::ownership::lock(&root.join("client.lock"))?;
            let id = job.unwrap_or(fs::read_to_string(root.join("last-job"))?);
            ensure!(
                id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
                "invalid job ID"
            );
            let dir = root.join("jobs").join(id);
            let job: Job = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
            ws.identity()?.authorize(&job.snapshot.identity.owner)?;
            let transport = Transport::connect()?;
            transport::install_cancel_handler()?;
            wait(ws, &transport, &job, &dir, cancel)
        }
    }
}

fn validate(command: &Command) -> Result<()> {
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

fn wait(ws: &Workspace, transport: &Transport, job: &Job, dir: &Path, cancel: bool) -> Result<()> {
    let owner = &job.snapshot.identity.owner;
    let mut cancelled = false;
    let mut offset = [0u64; 2];
    // Resume streams only bytes not already durably saved on Windows.
    for (i, name) in ["stdout", "stderr"].iter().enumerate() {
        offset[i] = fs::metadata(dir.join(format!("{name}.log")))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    let paths = matches!(
        job.command,
        Command::Disk(_) | Command::Tool(_) | Command::Export { .. }
    ) || matches!(&job.command, Command::Iso(a) if !a.check_registry && !a.due);
    let result = loop {
        if (cancel || transport::cancelled()) && !cancelled {
            transport.rpc(
                Rpc::Cancel {
                    owner,
                    job: &job.job,
                },
                &[],
            )?;
            cancelled = true;
            eprintln!("ilex: cancelled job {}; builder remains running", job.job);
        }
        let status = transport.rpc(Rpc::Status { owner, job: &job.job }, &[])
            .with_context(|| format!("job {} retained; reconnect with `cargo xtask remote resume --job {}`; logs at {}", job.job, job.job, dir.display()))?.0;
        for (i, name) in ["stdout", "stderr"].iter().enumerate() {
            let end = status["logs"][*name].as_u64().unwrap_or(0);
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
                ensure!(
                    !bytes.is_empty() && reply["sha256"] == digest(&bytes),
                    "log transfer checksum mismatch"
                );
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(format!("{name}.log")))?;
                file.write_all(&bytes)?;
                if i == 0 && !paths {
                    std::io::stdout().write_all(&bytes)?;
                } else {
                    std::io::stderr().write_all(&bytes)?;
                }
                offset[i] += bytes.len() as u64;
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
    fs::write(dir.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    // Collect even when the Linux command or cancellation failed.
    let destinations: Value = serde_json::from_slice(&fs::read(dir.join("destinations.json"))?)?;
    let mut delivered = BTreeMap::new();
    let primary_name = fs::read_to_string(dir.join("stdout.log"))
        .ok()
        .and_then(|text| {
            text.lines()
                .last()
                .map(|line| line.rsplit('/').next().unwrap_or(line).to_owned())
        });
    for artifact in result["artifacts"]
        .as_array()
        .context("remote artifact manifest")?
    {
        let name = artifact["path"].as_str().context("artifact has no path")?;
        let destination = if let Some(tail) = name.strip_prefix("job/") {
            safe_join(dir, tail)?
        } else if name.starts_with("disks/")
            && matches!(
                Path::new(name).extension().and_then(|e| e.to_str()),
                Some("qcow2" | "raw")
            )
        {
            let hash = artifact["sha256"]
                .as_str()
                .context("disk checksum missing")?;
            ensure!(
                hash.len() == 71
                    && hash.starts_with("sha256:")
                    && hash[7..].bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid disk checksum"
            );
            let destination = ws.state_dir().join("disks").join(format!(
                "{}.{}",
                &hash[7..],
                Path::new(name).extension().unwrap().to_string_lossy()
            ));
            reject_reparse(&destination)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            ilex_devkit::ownership::claim_file(ws, &destination)?;
            destination
        } else {
            safe_join(&ws.state_dir(), name)?
        };
        fetch(transport, job, artifact, &destination)?;
        delivered.insert(name.to_owned(), destination.clone());
        if name.starts_with("exports/")
            && primary_name
                .as_ref()
                .is_some_and(|base| name == format!("exports/{base}"))
            && let Some(out) = destinations["archive"].as_str()
        {
            copy_atomic(&destination, Path::new(out))?;
        }
        if let Some(tail) = name.strip_prefix("artifacts/")
            && let Some(root) = destinations["artifacts"].as_str()
        {
            copy_atomic(&destination, &safe_join(Path::new(root), tail)?)?;
        }
        if let Some(tail) = name.strip_prefix("iso/")
            && let Some(root) = destinations["iso"].as_str()
        {
            copy_atomic(&destination, &safe_join(Path::new(root), tail)?)?;
        }
        if name.starts_with("disks/")
            && primary_name
                .as_ref()
                .is_some_and(|base| name == format!("disks/{base}"))
            && matches!(
                destination.extension().and_then(|e| e.to_str()),
                Some("qcow2" | "raw")
            )
            && let Some(out) = destinations["disk"].as_str()
        {
            copy_atomic(&destination, Path::new(out))?;
        }
        eprintln!(
            "ilex: artifact {} ({})",
            destination.display(),
            artifact["sha256"]
        );
    }
    fs::write(
        dir.join("delivered.json"),
        serde_json::to_vec_pretty(&delivered)?,
    )?;
    if matches!(job.command, Command::Disk(_))
        && let Some(base) = primary_name.as_ref()
        && let Some(disk) = delivered.get(&format!("disks/{base}"))
        && let Some(metadata) = delivered.get(&format!("disks/{base}.disk.json"))
    {
        let mut artifact: ilex_devkit::core::DiskArtifact =
            serde_json::from_slice(&fs::read(metadata)?)?;
        ilex_devkit::core::check_version(artifact.schema)?;
        ensure!(
            artifact.location.sha256 == hash_file(disk)?,
            "installed disk metadata checksum mismatch"
        );
        artifact.location.path = disk.clone();
        artifact.location.executor_id = format!("windows-owner:{}", job.snapshot.identity.owner);
        artifact.allocated_bytes = transport::allocated_bytes(disk)?;
        fs::write(
            disk.with_file_name(format!(
                "{}.disk.json",
                disk.file_name().unwrap().to_string_lossy()
            )),
            serde_json::to_vec_pretty(&artifact)?,
        )?;
    }
    eprintln!("ilex: job {} evidence: {}", job.job, dir.display());
    let code = result["exit_code"]
        .as_i64()
        .context("missing remote exit code")?;
    if code != 0 {
        let code = if code < 0 { 128 - code } else { code };
        return Err(super::Failure {
            code: u8::try_from(code).unwrap_or(1),
            message: format!(
                "remote job {} exited {code}; artifacts retrieved to {}",
                job.job,
                ws.state_dir().display()
            ),
        }
        .into());
    }
    if paths {
        let stdout = fs::read_to_string(dir.join("stdout.log"))?;
        if let Some(path) = stdout.lines().last() {
            let basename = path.rsplit('/').next().context("remote output basename")?;
            let location = match job.command {
                Command::Disk(_) => destinations["disk"]
                    .as_str()
                    .map(PathBuf::from)
                    .or_else(|| delivered.get(&format!("disks/{basename}")).cloned())
                    .context("installed disk was not delivered")?,
                Command::Iso(_) => destinations["iso"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or(ws.state_dir().join("iso"))
                    .join(basename),
                Command::Export { .. } => destinations["archive"]
                    .as_str()
                    .map(PathBuf::from)
                    .or_else(|| delivered.get(&format!("exports/{basename}")).cloned())
                    .context("image archive was not delivered")?,
                _ => ws.state_dir().join("tools/images").join(basename),
            };
            println!("{}", location.display());
        }
    }
    Ok(())
}

fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.contains(['\\', ':'])
            && !Path::new(name).is_absolute()
            && Path::new(name)
                .components()
                .all(|p| matches!(p, Component::Normal(_))),
        "unsafe artifact path"
    );
    for part in name.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
        ensure!(
            !part.ends_with([' ', '.'])
                && !matches!(
                    stem.as_str(),
                    "con"
                        | "prn"
                        | "aux"
                        | "nul"
                        | "com1"
                        | "com2"
                        | "com3"
                        | "com4"
                        | "com5"
                        | "com6"
                        | "com7"
                        | "com8"
                        | "com9"
                        | "lpt1"
                        | "lpt2"
                        | "lpt3"
                        | "lpt4"
                        | "lpt5"
                        | "lpt6"
                        | "lpt7"
                        | "lpt8"
                        | "lpt9"
                ),
            "invalid Windows artifact name"
        );
    }
    let output = root.join(name);
    reject_reparse(&output)?;
    Ok(output)
}
fn reject_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(ancestor) {
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "reparse point in output path: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}
fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut f = fs::File::open(path)?;
    let mut b = [0; 128 * 1024];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("sha256:{:x}", h.finalize()))
}
fn fetch(transport: &Transport, job: &Job, artifact: &Value, destination: &Path) -> Result<()> {
    let bytes = artifact["bytes"]
        .as_u64()
        .context("invalid artifact size")?;
    let hash = artifact["sha256"]
        .as_str()
        .context("invalid artifact hash")?;
    if destination.is_file()
        && fs::metadata(destination)?.len() == bytes
        && hash_file(destination)? == hash
    {
        return Ok(());
    }
    ensure!(
        transport::available_space(destination)? >= bytes + 30 * 1024 * 1024 * 1024,
        "artifact transfer requires its size plus 30 GiB free on Windows"
    );
    reject_reparse(destination)?;
    fs::create_dir_all(destination.parent().context("output parent missing")?)?;
    let temporary = destination.with_file_name(format!(
        "{}.{}.partial",
        destination.file_name().unwrap().to_string_lossy(),
        job.job
    ));
    reject_reparse(&temporary)?;
    let mut f = fs::File::create(&temporary)?;
    let mut offset = 0u64;
    while offset < bytes {
        let (reply, data) = transport.rpc(
            Rpc::Fetch {
                owner: &job.snapshot.identity.owner,
                job: &job.job,
                path: artifact["path"].as_str().context("artifact path missing")?,
                offset,
            },
            &[],
        )?;
        ensure!(
            !data.is_empty() && reply["sha256"] == digest(&data),
            "artifact chunk mismatch"
        );
        f.write_all(&data)?;
        offset += data.len() as u64;
    }
    f.sync_all()?;
    drop(f);
    ensure!(
        offset == bytes && hash_file(&temporary)? == hash,
        "artifact checksum mismatch; partial retained"
    );
    fs::rename(temporary, destination)?;
    Ok(())
}
fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    if std::path::absolute(source)? == std::path::absolute(destination)? {
        return Ok(());
    }
    reject_reparse(destination)?;
    ensure!(
        transport::available_space(destination)?
            >= fs::metadata(source)?.len() + 30 * 1024 * 1024 * 1024,
        "explicit output requires its size plus 30 GiB free"
    );
    fs::create_dir_all(
        destination
            .parent()
            .context("explicit output has no parent")?,
    )?;
    let partial = destination.with_file_name(format!(
        "{}.partial",
        destination.file_name().unwrap().to_string_lossy()
    ));
    reject_reparse(&partial)?;
    fs::copy(source, &partial)?;
    ensure!(
        hash_file(&partial)? == hash_file(source)?,
        "output copy checksum mismatch"
    );
    fs::rename(partial, destination)?;
    Ok(())
}
