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
    let tmpfiles = bcvk::image_file_sha256(&base, "/usr/lib/tmpfiles.d/ilex-var.conf")?;
    let image = Image(machine::build(&ws, &base, &directory, None, &[])?);
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
    let actual = vm
        .ssh("sha256sum /usr/lib/tmpfiles.d/ilex-var.conf")?
        .ok("base tmpfiles hash")?;
    ensure(
        actual.out().split_whitespace().next() == tmpfiles.split_whitespace().next(),
        || "machine layer changed base tmpfiles".into(),
    )?;
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
        "fixture token {token}; machine JSON, label, overlay, kargs, self-check and base tmpfiles match"
    );
    Ok(())
}
