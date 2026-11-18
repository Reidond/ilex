//! Flatpak owns preinstall bookkeeping; ilex schedules it and reports completeness.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const STATE: &str = "/var/lib/ilex/apps/state.json";
const STATUS: &str = "/run/ilex/apps/status";
const REPO: &str = "/var/lib/flatpak/repo/config";
const REMOTE: &str = "https://dl.flathub.org/repo/";
const LIST_DIRS: [&str; 2] = [
    "/usr/share/flatpak/preinstall.d",
    "/etc/flatpak/preinstall.d",
];
type Result<T> = std::result::Result<T, String>;

#[derive(clap::Args)]
pub struct AppsArgs {
    #[command(subcommand)]
    command: AppsCommand,
}

#[derive(clap::Subcommand)]
enum AppsCommand {
    /// Reconcile the image's system apps (root; incomplete work exits 75)
    Sync {
        #[arg(long)]
        update: bool,
        /// Describe the work without changing files or contacting Flathub
        #[arg(long)]
        dry_run: bool,
    },
    /// Show wanted, installed, declined and unavailable apps
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Check the image's generated lists and timer
    #[command(long_flag = "self-check")]
    Check,
    #[command(hide = true)]
    Notify,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    schema: u32,
    result: String,
    detail: String,
    fingerprint: String,
    last_update: u64,
    initial_install_complete: bool,
    free_bytes: u64,
    apps: Vec<App>,
    updates: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
struct App {
    id: String,
    reference: String,
    status: String,
    origin: Option<String>,
}

pub fn run(args: AppsArgs) -> ExitCode {
    let result = match args.command {
        AppsCommand::Sync { update, dry_run } => sync(update, dry_run),
        AppsCommand::Status { json } => status(json).map(|_| 0),
        AppsCommand::Check => check().map(|_| {
            println!("apps self-check: ok");
            0
        }),
        AppsCommand::Notify => notify().map(|_| 0),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ilexctl: error: apps: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &str) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{path}: {e}"))?)
        .map_err(|e| e.to_string())
}

fn previous() -> Result<State> {
    match fs::read(STATE) {
        Ok(bytes) => {
            let state: State = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if state.schema != 1 {
                return Err("unsupported apps state schema".into());
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State {
            schema: 1,
            ..State::default()
        }),
        Err(e) => Err(e.to_string()),
    }
}

fn files(dir: &str) -> Result<Vec<PathBuf>> {
    let mut paths = match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{dir}: {e}")),
    };
    paths.retain(|p| p.extension().is_some_and(|e| e == "preinstall"));
    paths.sort();
    Ok(paths)
}

// GLib key files: the keys used here are ASCII scalars. Preserve complete
// groups so an /etc group overrides /usr exactly as Flatpak does.
fn ini(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut groups = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_string();
            groups.entry(section.clone()).or_default();
        } else if let Some((key, value)) = line.split_once('=') {
            groups
                .entry(section.clone())
                .or_default()
                .insert(key.trim().into(), value.trim().into());
        }
    }
    groups
}

fn wanted() -> Result<(BTreeMap<String, String>, String)> {
    let mut groups = BTreeMap::new();
    let mut hash = Sha256::new();
    for path in ["/usr/share/ilex/image.json", "/usr/share/ilex/machine.json"] {
        hash.update(fs::read(path).map_err(|e| e.to_string())?);
    }
    for dir in LIST_DIRS {
        for path in files(dir)? {
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            hash.update(path.as_os_str().to_string_lossy().as_bytes());
            hash.update([0]);
            hash.update(&bytes);
            groups.extend(ini(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?));
        }
    }
    let arch = std::env::consts::ARCH;
    let refs = groups
        .into_iter()
        .filter_map(|(group, values)| {
            let id = group.strip_prefix("Flatpak Preinstall ")?;
            if values.get("Install").is_some_and(|v| v == "false") {
                return None;
            }
            let kind = if values.get("IsRuntime").is_some_and(|v| v == "true") {
                "runtime"
            } else {
                "app"
            };
            let branch = values.get("Branch").map(String::as_str).unwrap_or("master");
            Some((format!("{kind}/{id}/{arch}/{branch}"), id.to_owned()))
        })
        .collect();
    Ok((refs, format!("{:x}", hash.finalize())))
}

fn flatpak(args: &[&str]) -> Result<Output> {
    Command::new("timeout")
        .args([
            if args
                .first()
                .is_some_and(|a| matches!(*a, "remote-ls" | "remote-info"))
            {
                "60"
            } else {
                "1800"
            },
            "flatpak",
            "--system",
        ])
        .args(args)
        .env("LC_ALL", "C.UTF-8")
        .output()
        .map_err(|e| e.to_string())
}

fn ok(args: &[&str]) -> Result<String> {
    let out = flatpak(args)?;
    if !out.status.success() {
        return Err(format!(
            "flatpak {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into())
}

fn installed() -> Result<BTreeMap<String, (String, String)>> {
    if !Path::new(REPO).exists() {
        return Ok(BTreeMap::new());
    }
    let mut refs = BTreeMap::new();
    for (kind, flag) in [("app", "--app"), ("runtime", "--runtime")] {
        let text = ok(&["list", flag, "--columns=ref,origin,active"])?;
        for line in text.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 3 {
                return Err(format!("unexpected flatpak list row: {line}"));
            }
            refs.insert(
                format!("{kind}/{}", fields[0]),
                (fields[1].into(), fields[2].into()),
            );
        }
    }
    Ok(refs)
}

fn classify(refs: &BTreeMap<String, String>) -> Result<Vec<App>> {
    let installed = installed()?;
    let config = fs::read_to_string(REPO).unwrap_or_default();
    let groups = ini(&config);
    let recorded = groups
        .get("core")
        .and_then(|g| g.get("xa.preinstalled"))
        .map(String::as_str)
        .unwrap_or("");
    Ok(refs
        .iter()
        .map(|(reference, id)| {
            let entry = installed.get(reference);
            let declined = recorded.split(';').any(|s| s == reference);
            App {
                id: id.clone(),
                reference: reference.clone(),
                origin: entry.map(|e| e.0.clone()),
                status: if entry.is_some() {
                    "installed"
                } else if declined {
                    "declined"
                } else {
                    "missing"
                }
                .into(),
            }
        })
        .collect())
}

fn save(state: &State) -> Result<()> {
    for path in [STATE, STATUS] {
        let path = Path::new(path);
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let temp = path.with_extension("json.tmp");
        fs::write(
            &temp,
            serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(temp, path).map_err(|e| e.to_string())?;
    }
    println!("apps: {}: {}", state.result, state.detail);
    Ok(())
}

fn defer(state: &mut State, result: &str, detail: impl Into<String>) -> Result<u8> {
    state.result = result.into();
    state.detail = detail.into();
    save(state)?;
    Ok(75)
}

fn sync(update: bool, dry_run: bool) -> Result<u8> {
    let (refs, fingerprint) = wanted()?;
    println!(
        "wanted: {}",
        refs.values().cloned().collect::<Vec<_>>().join(", ")
    );
    if dry_run {
        println!(
            "plan: check flathub URL/enabled state; hide system catalogue; reserve 8 GiB initially, 2 GiB later; probe remote; preinstall{}",
            if update {
                "; daily update (20h minimum)"
            } else {
                ""
            }
        );
        if !Path::new("/run/ostree-booted").exists() {
            println!("not an installed system (installer may invoke sync explicitly)");
        }
        return Ok(0);
    }
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| e.to_string())?;
    if String::from_utf8_lossy(&uid.stdout).trim() != "0" {
        return Err("sync requires root".into());
    }
    fs::create_dir_all("/var/lib/ilex/apps").map_err(|e| e.to_string())?;
    let lock = File::create("/var/lib/ilex/apps/sync.lock").map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut state = previous()?;
    state.apps = classify(&refs)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let due = update && now.saturating_sub(state.last_update) >= 20 * 60 * 60;
    // An empty initial list is inert. A formerly populated list must still
    // reach preinstall so Flatpak removes apps from the previous shell.
    if refs.is_empty() && !Path::new(REPO).exists() {
        state.result = "complete".into();
        state.detail = "no system apps requested".into();
        state.fingerprint = fingerprint;
        save(&state)?;
        return Ok(0);
    }
    // Opening the system installation applies remotes.d once, offline.
    ok(&["remotes", "--columns=name"])?;
    let config = ini(&fs::read_to_string(REPO).map_err(|e| e.to_string())?);
    // Disabling Fedora's add-remote unit does not remove the remote that an
    // older deployment already created in /var. Keep it out of preinstall's
    // search across enabled remotes, also after a bootc switch.
    if config
        .get("remote \"fedora\"")
        .is_some_and(|r| r.get("xa.disable").map(String::as_str) != Some("true"))
    {
        ok(&["remote-modify", "--disable", "fedora"])?;
    }
    let Some(remote) = config.get("remote \"flathub\"") else {
        state.result = "disabled".into();
        state.detail = "administrator removed flathub".into();
        save(&state)?;
        return Ok(0);
    };
    if remote.get("xa.disable").is_some_and(|v| v == "true")
        || remote.get("url").map(String::as_str) != Some(REMOTE)
    {
        state.result = "disabled".into();
        state.detail = "flathub is disabled or its URL was changed".into();
        save(&state)?;
        return Ok(0);
    }
    // Software 51 filters a no-enumerate catalogue only when main-ref is set.
    // An older installation may already have downloaded the full catalogue.
    // Remove only that regenerable system cache before notifying clients via
    // remote-modify. Installed apps, trust and user catalogues stay intact.
    let catalogue = Path::new("/var/lib/flatpak/appstream/flathub");
    let cleared_catalogue = catalogue.exists();
    if cleared_catalogue {
        fs::remove_dir_all(catalogue)
            .map_err(|e| format!("clear system Flathub catalogue: {e}"))?;
    }
    // Hide catalogue discovery; polkit is the authority for all mutations.
    if remote.get("xa.noenumerate").map(String::as_str) != Some("true") || cleared_catalogue {
        ok(&["remote-modify", "--no-enumerate", "flathub"])?;
    }
    if state.fingerprint == fingerprint && state.result == "complete" && !due {
        println!("apps: unchanged; no network work");
        return Ok(0);
    }
    let free = Command::new("df")
        .args(["--output=avail", "-B1", "/var/lib/flatpak"])
        .output()
        .map_err(|e| e.to_string())?;
    state.free_bytes = String::from_utf8_lossy(&free.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    let reserve = if state.initial_install_complete { 2 } else { 8 } * 1024_u64.pow(3);
    if state.free_bytes < reserve {
        return defer(
            &mut state,
            "space",
            format!("need {reserve} free bytes; retry later"),
        );
    }
    let missing = state.apps.iter().any(|a| a.status == "missing");
    if missing || due {
        let probe = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--head",
                "--connect-timeout",
                "10",
                "--max-time",
                "30",
                "https://dl.flathub.org/repo/summary",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !probe.status.success() {
            return defer(
                &mut state,
                "offline",
                String::from_utf8_lossy(&probe.stderr).trim(),
            );
        }
    }
    let mut available = BTreeSet::new();
    let mut runtimes = BTreeSet::new();
    let pending: Vec<_> = state
        .apps
        .iter()
        .filter(|a| a.status == "missing")
        .map(|a| a.reference.clone())
        .collect();
    for reference in pending {
        let probe = flatpak(&["remote-info", "--show-metadata", "flathub", &reference])?;
        if probe.status.success() {
            if let Some(runtime) = ini(&String::from_utf8_lossy(&probe.stdout)).get("Application").and_then(|a| a.get("runtime")) {
                runtimes.insert(format!("runtime/{runtime}"));
            }
            available.insert(reference);
        } else if !String::from_utf8_lossy(&probe.stderr).contains("No such ref") {
            return defer(
                &mut state,
                "retry",
                String::from_utf8_lossy(&probe.stderr).trim(),
            );
        }
    }
    // no-enumerate is also excluded by Flatpak's dependency-remote search.
    // Explicit runtime refs still work. Discover them from signed app metadata
    // without ever making the system catalogue enumerable, including runtime
    // transitions in manually installed apps on the daily update path.
    let deployed = installed()?;
    if due {
        for (reference, (origin, _)) in &deployed {
            if !reference.starts_with("app/") || origin != "flathub" { continue; }
            let metadata = flatpak(&["remote-info", "--show-metadata", "flathub", reference])?;
            if !metadata.status.success() {
                if String::from_utf8_lossy(&metadata.stderr).contains("No such ref") { continue; }
                return defer(&mut state, "retry", String::from_utf8_lossy(&metadata.stderr).trim());
            }
            if let Some(runtime) = ini(&String::from_utf8_lossy(&metadata.stdout)).get("Application").and_then(|a| a.get("runtime")) {
                runtimes.insert(format!("runtime/{runtime}"));
            }
        }
    }
    runtimes.retain(|runtime| !deployed.contains_key(runtime));
    if missing {
        state.result = "installing".into();
        state.detail = format!(
            "Installing {} system apps",
            state.apps.iter().filter(|a| a.status == "missing").count()
        );
        save(&state)?;
    }
    if !runtimes.is_empty() {
        let mut arguments = vec!["install", "--runtime", "--no-auto-pin", "-y", "--noninteractive", "--", "flathub"];
        arguments.extend(runtimes.iter().map(String::as_str));
        let result = flatpak(&arguments)?;
        if !result.status.success() {
            return defer(&mut state, "retry", String::from_utf8_lossy(&result.stderr).trim());
        }
    }
    let result = flatpak(&["preinstall", "-y", "--noninteractive"])?;
    if !result.status.success() {
        return defer(
            &mut state,
            "retry",
            String::from_utf8_lossy(&result.stderr).trim(),
        );
    }
    state.apps = classify(&refs)?;
    if state
        .apps
        .iter()
        .any(|a| a.status == "missing" && available.contains(&a.reference))
    {
        return defer(
            &mut state,
            "retry",
            "Flathub has requested apps that did not finish installing",
        );
    }
    let unavailable = state.apps.iter().any(|a| a.status == "missing");
    for app in &mut state.apps {
        if app.status == "missing" {
            app.status = "unavailable".into();
        }
    }
    if due {
        let before = installed()?;
        let result = flatpak(&["update", "-y", "--noninteractive"])?;
        // Record applied refs even when a later operation in the transaction
        // failed. Include runtimes, which ilex updates as well as applications.
        let after = installed()?;
        state.updates = after.iter().filter_map(|(reference, (origin, commit))| {
            let old = before.get(reference)?;
            (old.1 != *commit).then(|| json!({"kind":"flatpak", "ref":reference, "origin":origin, "from":old.1, "to":commit, "applied_at":now}))
        }).collect();
        if !result.status.success() {
            return defer(
                &mut state,
                "retry",
                String::from_utf8_lossy(&result.stderr).trim(),
            );
        }
        state.last_update = now;
    }
    state.result = if unavailable { "degraded" } else { "complete" }.into();
    state.detail = if unavailable {
        "Some requested apps are unavailable; see apps status".into()
    } else {
        "Apps are ready".into()
    };
    if !unavailable {
        state.fingerprint = fingerprint;
        state.initial_install_complete |= state.apps.iter().any(|a| a.status == "installed");
    }
    save(&state)?;
    Ok(0)
}

fn status(json_output: bool) -> Result<()> {
    let mut state = previous()?;
    let (refs, _) = wanted()?;
    let mut apps = classify(&refs)?;
    for app in &mut apps {
        if app.status == "missing"
            && state
                .apps
                .iter()
                .any(|old| old.reference == app.reference && old.status == "unavailable")
        {
            app.status = "unavailable".into();
        }
    }
    state.apps = apps;
    if state.result.is_empty() {
        state.result = "pending".into();
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&state).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "{}: {} ({} bytes free at last sync)",
            state.result, state.detail, state.free_bytes
        );
        for app in state.apps {
            println!(
                "{}: {} ({})",
                app.id,
                app.status,
                app.origin.as_deref().unwrap_or("no origin")
            );
        }
    }
    Ok(())
}

pub fn check() -> Result<()> {
    let image = read("/usr/share/ilex/image.json")?;
    let machine = read("/usr/share/ilex/machine.json")?;
    let mut expected = BTreeSet::new();
    for entry in image["shells"]
        .as_array()
        .ok_or("shells missing")?
        .iter()
        .chain(std::iter::once(&machine))
    {
        if let Some(ids) = entry["apps"]["flatpak"].as_array() {
            for id in ids {
                expected.insert(id.as_str().ok_or("invalid app ID")?.to_string());
            }
        }
    }
    let mut actual = BTreeSet::new();
    for path in files("/usr/share/ilex/apps")? {
        let bytes = fs::read(&path).map_err(|e| e.to_string())?;
        let installed = Path::new(LIST_DIRS[0]).join(format!(
            "50-ilex-{}",
            path.file_name().unwrap().to_string_lossy()
        ));
        if fs::read(&installed).map_err(|e| e.to_string())? != bytes {
            return Err(format!("{} differs", installed.display()));
        }
        for (group, values) in ini(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?) {
            let id = group
                .strip_prefix("Flatpak Preinstall ")
                .ok_or("invalid preinstall group")?;
            if values.get("Branch").map(String::as_str) != Some("stable")
                || values.contains_key("CollectionID")
            {
                return Err("preinstall must use stable without CollectionID".into());
            }
            actual.insert(id.to_string());
        }
    }
    if actual != expected {
        return Err(format!(
            "preinstall IDs differ: expected {expected:?}, found {actual:?}"
        ));
    }
    let enabled = Path::new("/etc/systemd/system/timers.target.wants/ilex-apps.timer").exists();
    if enabled == image["shells"].as_array().unwrap().is_empty() {
        return Err("apps timer differs from shell selection".into());
    }
    if previous()?.result == "degraded" {
        return Err("unavailable apps; run ilexctl apps status".into());
    }
    Ok(())
}

fn notify() -> Result<()> {
    let state: State = serde_json::from_slice(&fs::read(STATUS).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if !matches!(
        state.result.as_str(),
        "installing" | "complete" | "degraded"
    ) {
        return Ok(());
    }
    let body = if state.updates.is_empty() {
        state.detail
    } else {
        format!(
            "{}\nUpdated: {}",
            state.detail,
            state
                .updates
                .iter()
                .filter_map(|v| v["ref"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Command::new("notify-send")
        .args([
            "--app-name=ilex",
            "--icon=software-update-available",
            "--",
            "ilex apps",
            &body,
        ])
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(clap::Args)]
pub struct UpdatesArgs {
    #[command(subcommand)]
    command: UpdatesCommand,
}

#[derive(clap::Subcommand)]
enum UpdatesCommand {
    /// List the staged OS image and the last system Flatpak update transaction
    List {
        #[arg(long)]
        json: bool,
    },
}

pub fn updates(args: UpdatesArgs) -> ExitCode {
    let UpdatesCommand::List { json: json_output } = args.command;
    let result = (|| -> Result<()> {
        let mut updates = previous()?.updates;
        let bootc = Command::new("bootc")
            .args(["status", "--json"])
            .output()
            .map_err(|e| e.to_string())?;
        if !bootc.status.success() {
            return Err(String::from_utf8_lossy(&bootc.stderr).into());
        }
        let bootc: Value = serde_json::from_slice(&bootc.stdout).map_err(|e| e.to_string())?;
        if !bootc["status"]["staged"].is_null() {
            updates.insert(
                0,
                json!({"kind":"os", "status":"staged", "deployment":bootc["status"]["staged"]}),
            );
        }
        if json_output {
            println!("{}", json!({"schema":1,"updates":updates}));
        } else {
            for update in updates {
                if update["kind"] == "os" {
                    let image = &update["deployment"]["image"]["image"];
                    println!(
                        "OS staged: {}",
                        image["image"]
                            .as_str()
                            .or_else(|| image.as_str())
                            .unwrap_or("ilex image; restart to apply")
                    );
                } else {
                    println!(
                        "Flatpak {}: {} -> {} (applied)",
                        update["ref"].as_str().unwrap_or("unknown ref"),
                        update["from"].as_str().unwrap_or("?"),
                        update["to"].as_str().unwrap_or("?")
                    );
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ilexctl: error: updates: {e}");
            ExitCode::FAILURE
        }
    }
}
