//! Local machine overlay fixture, built through the same helper as xtask.

use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use ilex_devkit::{
    bcvk::{self, EphemeralOpts, EphemeralVm},
    cmd, machine,
};
use libtest_mimic::Trial;
use std::os::unix::fs::{PermissionsExt, symlink};

use crate::common::{self, ensure, trial};

pub fn trials() -> Vec<Trial> {
    vec![trial("config::machine_fixture", fixture)]
}

struct Image(String);
impl Drop for Image {
    fn drop(&mut self) {
        let _ = Command::new("podman").args(["rmi", &self.0]).output();
    }
}

fn fixture() -> Result<()> {
    let ws = common::workspace()?;
    let base = common::image()?;
    let directory = ws.state_dir().join("config-fixture/fixture");
    fs::create_dir_all(directory.join("rootfs/usr/share/ilex-config-fixture"))?;
    fs::create_dir_all(directory.join("rootfs/usr/lib/bootc/kargs.d"))?;
    let token = format!(
        "ilex-fixture-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    fs::write(
        directory.join("machine.toml"),
        "name = \"fixture\"\ndescription = \"Local machine overlay fixture\"\n",
    )?;
    fs::write(
        directory.join("rootfs/usr/share/ilex-config-fixture/marker"),
        &token,
    )?;
    fs::write(
        directory.join("rootfs/usr/lib/bootc/kargs.d/20-fixture.toml"),
        "kargs = [\"ilex.fixture=1\"]\n",
    )?;
    let parent = parent_overlay(&ws, &base, &token)?;
    let image = Image(machine::build(&ws, &parent.0, &directory, None, &[])?);
    let vm = EphemeralVm::boot(&ws.vm_name("config"), &image.0, &EphemeralOpts::default())?;
    let marker = vm
        .ssh("cat /usr/share/ilex-config-fixture/marker")?
        .ok("fixture marker")?;
    ensure(marker.out() == token, || "fixture token differs".into())?;
    let kargs = vm
        .ssh("cat /usr/lib/bootc/kargs.d/20-fixture.toml")?
        .ok("fixture kargs")?;
    let parsed: toml::Table = kargs.stdout.parse()?;
    ensure(
        parsed["kargs"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0].as_str() == Some("ilex.fixture=1")),
        || "fixture kargs differ".into(),
    )?;
    let config = vm
        .ssh("ilex-agent config --json")?
        .ok("fixture configuration")?;
    let config: serde_json::Value = serde_json::from_str(&config.stdout)?;
    ensure(config["machine"]["name"] == "fixture", || {
        "machine must be fixture".into()
    })?;
    vm.ssh("ilex-agent --self-check")?
        .ok("fixture self-check")?;
    let inherited = vm
        .ssh("cat /usr/share/ilex-config-parent/entry")?
        .ok("inherited parent file after replay")?;
    ensure(inherited.out() == token, || {
        "inherited parent content differs after replay".into()
    })?;
    let existing = vm
        .ssh("cat /usr/share/ilex-config-parent-existing")?
        .ok("inherited file in existing directory")?;
    ensure(existing.out() == token, || {
        "inherited existing-directory file differs".into()
    })?;
    let modes = vm
        .ssh(
            "stat -c '%a %u:%g' /usr/share/ilex-config-parent /usr/share/ilex-config-parent/entry",
        )?
        .ok("inherited modes")?;
    ensure(modes.out().lines().collect::<Vec<_>>() == ["750 0:0", "750 0:0"], || {
        format!("inherited modes differ: {}", modes.out())
    })?;
    let link = vm
        .ssh("readlink /usr/share/ilex-config-parent/alias")?
        .ok("inherited relative symlink")?;
    ensure(link.out() == "entry", || {
        "inherited relative symlink differs".into()
    })?;
    vm.ssh("test -f /usr/lib/tmpfiles.d/ilex-var-machine.conf && systemd-tmpfiles --cat-config > /dev/null")?.ok("machine tmpfiles exist and parse")?;
    println!(
        "parent replay: new directory and file retain 0750 root:root, content, relative link and existing-directory file"
    );
    collision_rejected(&ws, &parent.0)?;
    let labels = cmd::output(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{index .Labels \"ilex.machine\"}}",
        &image.0,
    ]))?;
    ensure(labels == "fixture", || {
        "fixture machine label differs".into()
    })?;
    println!(
        "fixture token {token}; parent replay, collision rejection, machine JSON, label, overlay, kargs, self-check and tmpfiles pass"
    );
    Ok(())
}

/// Internal fixture construction, not a machine-layer exception: this keeps
/// generic's registered machine.json and labels and records a shell-style
/// overlay with the production installer. Existing shell overlays are replayed
/// too. No product CLI or Containerfile accepts stacking machines.
fn parent_overlay(ws: &ilex_devkit::Workspace, base: &str, token: &str) -> Result<Image> {
    ensure(machine::image_machine(base)? == "generic", || {
        "config:: requires a generic base image".into()
    })?;
    let context = ws.state_dir().join("config-fixture/parent-build");
    let root = context.join("payload/overlays/000");
    let added = root.join("usr/share/ilex-config-parent");
    fs::create_dir_all(&added)?;
    fs::write(added.join("entry"), token)?;
    fs::set_permissions(&added, fs::Permissions::from_mode(0o750))?;
    fs::set_permissions(added.join("entry"), fs::Permissions::from_mode(0o750))?;
    let alias = added.join("alias");
    if fs::symlink_metadata(&alias).is_ok() {
        fs::remove_file(&alias)?;
    }
    symlink("entry", alias)?;
    fs::write(root.join("usr/share/ilex-config-parent-existing"), token)?;
    let helper = ws.root.join("build/image_tree.py");
    fs::copy(&helper, context.join("image_tree.py"))?;
    fs::copy(&helper, context.join("payload/install-files.py"))?;
    fs::write(
        context.join("payload/overlays.json"),
        serde_json::to_vec(&serde_json::json!([
            {"name": "shells/config-parent/rootfs", "directory": "overlays/000"}
        ]))?,
    )?;
    fs::write(
        context.join("Containerfile"),
        r#"ARG BASE
FROM ${BASE}
COPY image_tree.py /tmp/ilex-fixture-prepare.py
RUN python3 -I -S /tmp/ilex-fixture-prepare.py --prepare --payload /usr/share/ilex/.machine-replay && rm /tmp/ilex-fixture-prepare.py
COPY payload/ /tmp/ilex-compose/
RUN python3 -I -S /tmp/ilex-compose/install-files.py --payload /tmp/ilex-compose --phase overlays --inherited /usr/share/ilex/.machine-replay --remove-payload
RUN bootc container lint --fatal-warnings
"#,
    )?;
    let image = Image(format!("{}.config-parent", ws.image_ref()));
    cmd::run(
        Command::new("podman")
            .args([
                "build",
                "--layers",
                "--squash",
                "--build-arg",
                &format!("BASE={}", bcvk::image_id(base)?),
                "--tag",
                &image.0,
            ])
            .arg(&context)
            .stdout(std::io::stderr()),
    )?;
    let registered = cmd::output(Command::new("podman").args([
        "run",
        "--rm",
        "--network=none",
        "--entrypoint",
        "cat",
        &image.0,
        "/usr/share/ilex/.overlays.json",
    ]))?;
    let records: serde_json::Value = serde_json::from_str(&registered)?;
    ensure(
        records.as_array().is_some_and(|records| {
            records.iter().any(|record| {
                record["name"] == "shells/config-parent/rootfs"
                    && record["entries"]["usr/share/ilex-config-parent/entry"]["sha256"].is_string()
            })
        }),
        || "parent has no non-empty shell overlay provenance".into(),
    )?;
    println!(
        "parent fixture: generic machine metadata retained; non-empty shell overlay record installed"
    );
    Ok(image)
}

fn collision_rejected(ws: &ilex_devkit::Workspace, base: &str) -> Result<()> {
    let directory = ws.state_dir().join("config-fixture/collision");
    fs::create_dir_all(directory.join("rootfs/usr/share/ilex-config-parent"))?;
    fs::write(
        directory.join("machine.toml"),
        "name = \"collision\"\ndescription = \"Parent collision fixture\"\n",
    )?;
    fs::write(
        directory.join("rootfs/usr/share/ilex-config-parent/entry"),
        "replacement",
    )?;
    let image = Image(format!("{}.config-collision", ws.image_ref()));
    let mut command = Command::new("podman");
    command.current_dir(&ws.root).args([
        "build",
        "--layers",
        "--squash",
        "--file",
        "machines/Containerfile",
        "--build-arg",
        &format!("BASE={}", bcvk::image_id(base)?),
        "--build-arg",
        "MACHINE=collision",
        "--build-context",
        &format!("machine={}", directory.display()),
        "--tag",
        &image.0,
        "build",
    ]);
    eprintln!("$ {}", cmd::display(&command));
    let output = command.output()?;
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = "machines/collision/rootfs: /usr/share/ilex-config-parent/entry: already exists; overlays only add files";
    ensure(
        !output.status.success() && diagnostic.contains(expected),
        || {
            format!(
                "parent collision must fail at add-only installation, got {}:\n{diagnostic}",
                output.status
            )
        },
    )?;
    println!("collision build refused: {expected}");
    Ok(())
}
