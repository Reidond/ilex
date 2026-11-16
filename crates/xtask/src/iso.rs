//! `cargo xtask iso`: build the ilex installer ISO.
//!
//! A network installer: `image-builder` turns `installer/Containerfile` (a
//! fedora-bootc image with Anaconda in it) into the ISO, and the kickstart
//! defaults make Anaconda install the newest installer composition from `image.toml`
//! straight from the registry (`--source-imgref registry:…`) and track it
//! afterwards (`--target-imgref`). The ISO carries no ilex image, so it stays
//! current by itself and fits GitHub's 2 GiB limit per release asset.
//!
//! Next to the ISO this writes `ilex-installer.iso.sha256` and
//! `release-notes.md`; the workflow publishes the three as a GitHub Release
//! (main only), and a branch run uploads them as a workflow artifact.
//!
//! `--due` builds nothing: it says whether a new ISO is worth building (and
//! writes `iso_due=` to `$GITHUB_OUTPUT`) by looking at the latest
//! `installer-*` release: none yet, `installer/` (or this module) changed
//! since it, or it is a week old (to pick up Fedora and Anaconda fixes).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd, installer};
use sha2::{Digest, Sha256};

use crate::registry;

/// Basename of the ISO that is built and released.
const ISO_NAME: &str = "ilex-installer";
/// Repo paths (relative to the checkout) that make one ISO differ from the
/// next.
const INSTALLER_INPUTS: &[&str] = &[
    "installer",
    "crates/xtask/src/iso.rs",
    "crates/ilex-devkit/src/installer.rs",
    "crates/ilex-devkit/src/config.rs",
    // Future enrollment helpers live under installer/ and are covered above.
    "rootfs/etc/containers/registries.d/ilex.yaml",
    "rootfs/etc/pki/containers/ilex.pub",
];
/// The line of a release's notes that says what the installer was built from.
const FINGERPRINT_LINE: &str = "installer-fingerprint: ";
/// Release tags are `installer-YYYYMMDD` (with a `-N` suffix on a second one
/// the same day).
const RELEASE_TAG_PREFIX: &str = "installer-";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Directory the ISO is written to (default: `<state dir>/iso`)
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Local only: install this signed ilex tag for every composition
    #[arg(long)]
    pub testing_tag: Option<String>,
    /// Check that every offered composition and its signature tag exist
    #[arg(long, conflicts_with_all = ["due", "testing_tag"])]
    pub check_registry: bool,
    /// Don't build: say whether a new ISO is due, from the latest
    /// `installer-*` release of `--repository`
    #[arg(long)]
    pub due: bool,
    /// With `--due`: how old the latest installer release may get
    #[arg(long, default_value_t = 7)]
    pub max_age_days: u64,
    /// With `--due`: the GitHub repository (default `$GITHUB_REPOSITORY`)
    #[arg(long)]
    pub repository: Option<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let table = installer::table(&ws.root, args.testing_tag.as_deref())?;
    if args.check_registry {
        if !check_registry(&table)? {
            std::process::exit(3);
        }
        return Ok(());
    }
    if args.due {
        return due(ws, &args, &table);
    }
    cmd::require("podman")?;
    let iso = build(
        ws,
        &Build {
            testing_tag: args.testing_tag.as_deref(),
            output_dir: args.output_dir.as_deref(),
        },
    )?;
    println!("{}", iso.display());
    Ok(())
}

/// What to build an ISO from.
pub struct Build<'a> {
    pub testing_tag: Option<&'a str>,
    /// Where the ISO goes (default `<state dir>/iso`).
    pub output_dir: Option<&'a Path>,
}

/// Build the installer container and the ISO and return the path of the ISO;
/// its checksum and the release notes are written next to it.
pub fn build(ws: &Workspace, spec: &Build) -> Result<PathBuf> {
    let table = installer::table(&ws.root, spec.testing_tag)?;
    let out_dir = spec
        .output_dir
        .map_or_else(|| ws.state_dir().join("iso"), Path::to_path_buf);
    // Preserve existing outputs until a replacement is ready. In particular,
    // generated/ is a build context and must survive image-builder startup.
    fs::create_dir_all(&out_dir)?;
    let out_dir = std::path::absolute(&out_dir)?;
    let generated = out_dir.join("generated");
    fs::create_dir_all(&generated)?;
    fs::write(generated.join("compositions.json"), table_bytes(&table)?)?;
    fs::write(generated.join("iso.yaml"), installer::BOOT_MENU)?;
    fs::copy(
        ws.root.join("rootfs/etc/containers/registries.d/ilex.yaml"),
        generated.join("ilex.yaml"),
    )?;
    fs::copy(
        ws.root.join("rootfs/etc/pki/containers/ilex.pub"),
        generated.join("ilex.pub"),
    )?;
    let installer = format!("localhost/ilex-installer:{}", ws.slug);
    let context = ws.root.join("installer");
    cmd::run(
        Command::new("podman")
            // Host networking: in CI this runs podman inside a container,
            // where the nested bridge network has no DNS.
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args([
                "--build-context",
                &format!("generated={}", generated.display()),
            ])
            .args(["--tag", &installer])
            .arg(&context),
    )?;

    // image-builder turns the installer container into the ISO.
    let build_dir = out_dir.join("build");
    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)?;
    }
    fs::create_dir_all(&build_dir)?;
    run_image_builder(ws, &installer, &build_dir)?;

    // image-builder puts the ISO in a subdirectory; the release wants it flat.
    let built = find_iso(&build_dir)?;
    let iso = out_dir.join(format!("{ISO_NAME}.iso"));
    if built != iso {
        fs::rename(&built, &iso).with_context(|| format!("moving {}", built.display()))?;
    }
    let size = fs::metadata(&iso)?.len();
    let sha256 = sha256_file(&iso)?;
    fs::write(
        out_dir.join(format!("{ISO_NAME}.iso.sha256")),
        format!("{sha256}  {ISO_NAME}.iso\n"),
    )?;
    fs::write(
        out_dir.join("release-notes.md"),
        release_notes(&installer_fingerprint(ws, &table)?, &sha256, &table),
    )?;
    fs::copy(
        generated.join("compositions.json"),
        out_dir.join("compositions.json"),
    )?;
    eprintln!(
        "built {} ({:.2} GiB, sha256 {sha256})",
        iso.display(),
        size as f64 / (1u64 << 30) as f64
    );
    // A release asset has to stay under 2 GiB.
    if size >= 2 << 30 {
        bail!(
            "the ISO is {:.2} GiB; a GitHub release asset has to be under 2 GiB",
            size as f64 / (1u64 << 30) as f64
        );
    }
    Ok(iso)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(crate::rechunk::hex(&hash.finalize()))
}

fn release_notes(fingerprint: &str, sha256: &str, table: &installer::Table) -> String {
    let mut notes = format!(
        "Network installer for ilex. Write `{ISO_NAME}.iso` to a USB stick and boot it: the first screen \
         shows the detected machine and lets you choose the machine configuration and desktop. Press Enter to accept the defaults. \
         Anaconda's web interface then asks for the disk and a maintenance/recovery administrator account and installs the chosen signed composition over the network \
         (a wired connection is needed) and the machine updates itself from there.\n\n\
         SHA-256 of `{ISO_NAME}.iso`: `{sha256}`\n\n{FINGERPRINT_LINE}{fingerprint}\n"
    );
    notes.push_str(&format!("\nDefault: `{}`. Override with `ilex.composition=<tag>` at boot. The chooser is skipped for this override and for kickstart, text and remote installs.\n\nOffered compositions:\n", table.default));
    for entry in &table.compositions {
        notes.push_str(&format!(
            "- `{}`: {}\n",
            entry.tag, table.machines[&entry.machine].description
        ));
    }
    notes.push_str("\nMachine match tables (exact, case-sensitive, either complete pair):\n");
    for (name, machine) in &table.machines {
        for rule in &machine.matches {
            notes.push_str(&format!("- `{name}`: {rule:?}\n"));
        }
    }
    if let Some(tag) = &table.testing_tag {
        notes.push_str(&format!(
            "\n**Testing build: installs `{tag}` for every entry.**\n"
        ));
    }
    notes
}

fn table_bytes(table: &installer::Table) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(table)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Presence guard only. The installer verifies the signature itself; CI also
/// verifies the default digest with cosign before and after its install gate.
fn check_registry(table: &installer::Table) -> Result<bool> {
    cmd::require("skopeo")?;
    let mut available = true;
    for entry in &table.compositions {
        let reference = format!("docker://{}:{}", installer::REPOSITORY, entry.tag);
        let image = Command::new("skopeo")
            .args(["inspect", "--raw", &reference])
            .output()
            .with_context(|| format!("reading {reference}"))?;
        if !image.status.success() {
            let error = String::from_utf8_lossy(&image.stderr);
            if registry::looks_missing(&error) {
                println!("{}: not published yet", entry.tag);
                available = false;
                continue;
            }
            bail!("reading {reference}: {error}");
        }
        // Hash the returned manifest itself: a second tag lookup could move.
        let digest = format!("sha256:{:x}", Sha256::digest(&image.stdout));
        let signature = registry::capture(Command::new("skopeo").args([
            "inspect",
            "--raw",
            &format!(
                "docker://{}:{}.sig",
                installer::REPOSITORY,
                digest.replace(':', "-")
            ),
        ]))?;
        if !signature.ok {
            if registry::looks_missing(&signature.stderr) {
                println!("{}: signature not published yet ({digest})", entry.tag);
                available = false;
                continue;
            }
            bail!("reading signature for {reference}: {}", signature.stderr);
        }
        println!("{}: published with signature tag ({digest})", entry.tag);
    }
    Ok(available)
}

/// What the installer is built from: a hash of [`INSTALLER_INPUTS`].
fn installer_fingerprint(ws: &Workspace, table: &installer::Table) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(table_bytes(table)?);
    hash.update(installer::BOOT_MENU.as_bytes());
    for input in INSTALLER_INPUTS {
        hash.update(input.as_bytes());
        hash_tree(&ws.root, &ws.root.join(input), &mut hash)?;
    }
    Ok(format!("sha256:{}", crate::rechunk::hex(&hash.finalize())))
}

/// Hash the names and contents of everything below `path`, in sorted order.
fn hash_tree(root: &Path, path: &Path, hash: &mut Sha256) -> Result<()> {
    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            hash_tree(root, &entry, hash)?;
        }
    } else {
        let name = path.strip_prefix(root).unwrap_or(path);
        hash.update(name.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update(b"\0");
        hash.update(fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        hash.update(b"\0");
    }
    Ok(())
}

/// `--due`: is a new ISO worth building? Prints the answer and the reason and
/// writes `iso_due=true|false`.
fn due(ws: &Workspace, args: &Args, table: &installer::Table) -> Result<()> {
    let repository = args
        .repository
        .clone()
        .or_else(|| {
            std::env::var("GITHUB_REPOSITORY")
                .ok()
                .filter(|r| !r.is_empty())
        })
        .context("which GitHub repository? pass --repository or set GITHUB_REPOSITORY")?;
    cmd::require("gh")?;
    let (is_due, reason) = iso_due(ws, &repository, args.max_age_days, table)?;
    eprintln!("installer ISO due: {is_due} ({reason})");
    registry::github_output("iso_due", &is_due.to_string())
}

fn iso_due(
    ws: &Workspace,
    repository: &str,
    max_age_days: u64,
    table: &installer::Table,
) -> Result<(bool, String)> {
    let releases = registry::capture(
        Command::new("gh").args(["api", &format!("repos/{repository}/releases?per_page=30")]),
    )?;
    if !releases.ok {
        bail!(
            "`gh api repos/{repository}/releases` failed: {}",
            releases.stderr
        );
    }
    let releases: serde_json::Value =
        serde_json::from_str(&releases.stdout).context("parsing the releases")?;
    let latest = releases.as_array().into_iter().flatten().find(|r| {
        r["tag_name"]
            .as_str()
            .is_some_and(|t| t.starts_with(RELEASE_TAG_PREFIX))
            && r["draft"] != true
    });
    let Some(latest) = latest else {
        return Ok((true, "no installer release yet".into()));
    };
    let tag = latest["tag_name"].as_str().unwrap_or("?");

    let current = installer_fingerprint(ws, table)?;
    let published = latest["body"]
        .as_str()
        .unwrap_or("")
        .lines()
        .find_map(|l| l.strip_prefix(FINGERPRINT_LINE));
    eprintln!(
        "installer fingerprint: {current} (release {tag}: {})",
        published.unwrap_or("none")
    );
    if published != Some(current.as_str()) {
        return Ok((true, format!("the installer changed since {tag}")));
    }

    let Some(created) = latest["published_at"]
        .as_str()
        .and_then(registry::parse_rfc3339_utc)
    else {
        return Ok((true, format!("{tag} has no publication date")));
    };
    let age_days = registry::now_secs().saturating_sub(created) / 86_400;
    Ok(if age_days >= max_age_days {
        (true, format!("{tag} is {age_days} days old"))
    } else {
        (
            false,
            format!("{tag} is {age_days} days old and the installer is unchanged"),
        )
    })
}

/// The `generic-iso` image type (Anaconda inside the installer container,
/// kickstart from the container) needs image-builder 89 (Fedora 45 updates);
/// version 87's only installer type is the unattended,
/// disk-wiping `bootc-installer`.
const MIN_IMAGE_BUILDER: u32 = 89;

/// Major version of the image-builder on `PATH`, if there is one.
fn host_image_builder() -> Option<u32> {
    let out = cmd::probe(Command::new("image-builder").arg("version"))?;
    out.lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .and_then(|v| v.trim().split('.').next()?.parse().ok())
}

/// Run `image-builder build … generic-iso`: directly when the one on `PATH`
/// is new enough (CI), otherwise inside a privileged Fedora 45 container that
/// shares this host's container storage, so a dev machine with an older
/// image-builder (or none) works the same.
fn run_image_builder(ws: &Workspace, installer: &str, out_dir: &Path) -> Result<()> {
    let build_args = |out: &str| -> Vec<String> {
        [
            "build",
            "--bootc-ref",
            installer,
            // image-builder wants a default root filesystem for the installer
            // container; it isn't used for the machines Anaconda installs.
            "--bootc-default-fs",
            "ext4",
            "--output-name",
            ISO_NAME,
            "--output-dir",
            out,
            "generic-iso",
        ]
        .map(String::from)
        .to_vec()
    };

    if host_image_builder().is_some_and(|v| v >= MIN_IMAGE_BUILDER) {
        return cmd::run(
            Command::new("image-builder").args(build_args(&out_dir.to_string_lossy())),
        );
    }
    eprintln!(
        "xtask: image-builder {} on PATH; running version {MIN_IMAGE_BUILDER}+ in a container",
        host_image_builder().map_or("is not".to_string(), |v| format!("{v} is too old:")),
    );

    // The helper is checkout-owned too: concurrent ISO builds must not retag it.
    let helper = format!("localhost/ilex-image-builder:{}", ws.slug);
    let context = ws.root.join("installer").join("image-builder");
    cmd::run(
        Command::new("podman")
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args(["--tag", &helper])
            .arg(&context),
    )?;

    // It reads the installer from this host's container storage
    // and keeps osbuild's cache next to it; neither may sit on the
    // container's own overlayfs (podman can't nest overlay on overlay).
    let graph_root =
        cmd::output(Command::new("podman").args(["info", "--format", "{{.Store.GraphRoot}}"]))?;
    let storage = Path::new(&graph_root)
        .parent()
        .unwrap_or(Path::new("/var/lib/containers"));
    let cache = Path::new("/var/cache/image-builder");
    fs::create_dir_all(cache).with_context(|| format!("creating {}", cache.display()))?;
    let mut run = Command::new("podman");
    run.args(["run", "--rm", "--privileged", "--network=host"])
        .args(["--security-opt", "label=disable"])
        .arg("--volume")
        .arg(format!("{0}:{0}", storage.display()))
        .arg("--volume")
        .arg(format!("{0}:{0}", cache.display()))
        .arg("--volume")
        .arg(format!("{}:/out", out_dir.display()))
        .args(["--env", "TMPDIR=/var/cache/image-builder/tmp"])
        .arg(&helper)
        .args([
            "sh",
            "-c",
            "mkdir -p \"$TMPDIR\" && exec image-builder \"$@\"",
            "image-builder",
        ])
        .args(build_args("/out"));
    cmd::run(&mut run)
}

/// The `.iso` image-builder wrote somewhere below `dir`.
fn find_iso(dir: &Path) -> Result<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "iso") {
                return Ok(path);
            }
        }
    }
    bail!("image-builder produced no .iso under {}", dir.display())
}
