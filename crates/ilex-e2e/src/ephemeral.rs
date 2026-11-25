//! `ephemeral::` tier: The image booted directly as an ephemeral VM, no disk.
//!
//! One VM serves the whole tier. It comes from `bcvk ephemeral run`, holds a
//! machine-wide VM slot while it lives, and is removed when the last
//! `ephemeral::` test finishes (or, on a panic, when its guard drops).

use anyhow::{Context, Result};
use ilex_devkit::bcvk::{self, EphemeralOpts, EphemeralVm};
use libtest_mimic::Trial;
use serde_json::Value;

use crate::common::{self, Group, ensure, trial};

const ILEXCTL: &str = "/usr/bin/ilexctl";

static GROUP: Group<EphemeralVm> = Group::new("ephemeral", boot);

fn boot() -> Result<EphemeralVm> {
    let ws = common::workspace()?;
    let image = common::image()?;
    let name = ws.vm_name("ephemeral");
    let vm = EphemeralVm::boot(&name, &image, &EphemeralOpts::default())?;
    eprintln!(
        "ilex: {name} is up from {image} (VM slot {})",
        vm.slot_index()
    );
    Ok(vm)
}

/// How many `ephemeral::` tests will run, so the last one frees the VM.
pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

/// Release the VM now (safety net after the run).
pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    vec![
        trial("ephemeral::system_running", || GROUP.run(system_running)),
        trial("ephemeral::no_failed_units", || GROUP.run(no_failed_units)),
        trial("ephemeral::sshd_active", || GROUP.run(sshd_active)),
        trial("ephemeral::agent_self_check", || {
            GROUP.run(agent_self_check)
        }),
        trial("ephemeral::enrollment_layout", || GROUP.run(enrollment_layout)),
        trial("ephemeral::ilexctl", || GROUP.run(ilexctl)),
        trial("ephemeral::image_under_test", || {
            GROUP.run(image_under_test)
        }),
        trial("ephemeral::bootc_status", || GROUP.run(bootc_status)),
        trial("ephemeral::graphical_default", || {
            GROUP.run(graphical_default)
        }),
        trial("ephemeral::sigpolicy_noop", || GROUP.run(sigpolicy_noop)),
        trial("ephemeral::configuration", || GROUP.run(configuration)),
        trial("ephemeral::apps_plan", || GROUP.run(apps_plan)),
        trial("ephemeral::user_profiles", || GROUP.run(user_profiles)),
        trial("ephemeral::shell_themes", || GROUP.run(shell_themes)),
    ]
}

fn enrollment_layout(vm: &EphemeralVm) -> Result<()> {
    let script = ilex_devkit::qemu::base64(include_bytes!("encrypted/mixed-layout.sh"));
    let out = vm
        .ssh(&format!(
            "printf %s {script} | base64 -d | unshare --mount --propagation private bash -se"
        ))?
        .ok("mixed encrypted and plaintext layouts")?;
    println!("{}", out.stdout);
    Ok(())
}

fn ilexctl(vm: &EphemeralVm) -> Result<()> {
    let source = ilex_devkit::qemu::base64(include_bytes!("ilexctl-guest.py"));
    vm.ssh(&format!("printf %s {source} | base64 -d > /run/ilexctl-check.py"))?
        .ok("installing ilexctl guest checks")?;
    let out = vm.ssh("python3 -I -S /run/ilexctl-check.py")?
        .ok("ilexctl machine CLI")?;
    println!("     {}", out.stdout);
    Ok(())
}

fn apps_plan(vm: &EphemeralVm) -> Result<()> {
    let out = vm.ssh("ilexctl apps sync --dry-run")?.ok("apps dry run")?;
    ensure(
        out.stdout.contains("wanted:") && out.stdout.contains("not an installed system"),
        || out.stdout.clone(),
    )?;
    let out = vm
        .ssh("ilexctl apps status --json")?
        .ok("apps JSON status")?;
    let state: Value = serde_json::from_str(&out.stdout)?;
    ensure(state["schema"] == 1 && state["apps"].is_array(), || {
        "invalid apps status".into()
    })?;
    vm.ssh("test ! -e /var/lib/ilex/apps/state.json")?
        .ok("dry run must not write apps state")?;
    let source = ilex_devkit::qemu::base64(include_bytes!("apps-config-guest.py"));
    vm.ssh(&format!("printf %s {source} | base64 -d > /run/apps-config-check.py && python3 -I -S /run/apps-config-check.py"))?.ok("app manifests, trust and units")?;
    Ok(())
}


fn user_profiles(vm: &EphemeralVm) -> Result<()> {
    use ilex_devkit::qemu::base64;
    vm.ssh("mkdir -p /run/ilex-profiles-build")?
        .ok("profile fixture directory")?;
    for (name, source) in [
        ("compose.py", include_str!("../../../build/compose.py")),
        (
            "image_tree.py",
            include_str!("../../../build/image_tree.py"),
        ),
        ("check.py", include_str!("profiles-build-guest.py")),
    ] {
        vm.ssh(&format!(
            "printf %s {} | base64 -d > /run/ilex-profiles-build/{name}",
            base64(source.as_bytes())
        ))?
        .ok("profile build fixture")?;
    }
    let result = vm
        .ssh("timeout 120 python3 -I -S /run/ilex-profiles-build/check.py")?
        .ok("public profile regressions")?;
    println!("     {}", result.stdout);
    Ok(())
}

/// Each desktop keeps its own look. Nothing a shell ships may restyle another
/// shell's session, so GTK, portal and GSettings defaults are never global;
/// the KDE shell's GTK defaults are reachable only from a Plasma session.
fn shell_themes(vm: &EphemeralVm) -> Result<()> {
    let selection = common::selection()?;
    if selection.shells.is_empty() {
        return Ok(());
    }
    // No global GTK theme, portal selection or GSettings interface override.
    vm.ssh(
        r#"set -eu
for f in /etc/gtk-2.0/gtkrc /etc/gtk-3.0/settings.ini /etc/gtk-4.0/settings.ini \
         /etc/xdg/gtk-3.0/settings.ini /etc/xdg/gtk-4.0/settings.ini \
         /etc/xdg-desktop-portal/portals.conf /usr/share/xdg-desktop-portal/portals.conf; do
    test ! -e "$f" || { echo "global default $f"; exit 1; }
done
if grep -rIls -E '(^|[[:space:]])(export[[:space:]]+)?(GTK_THEME|GTK2_RC_FILES|DCONF_PROFILE|QT_QPA_PLATFORMTHEME)=' \
    /etc/environment /etc/environment.d /usr/lib/environment.d /etc/profile.d \
    /usr/lib/systemd/user.conf.d /etc/systemd/user.conf.d /usr/lib/systemd/user-environment-generators 2>/dev/null; then
    echo "a global environment default sets a toolkit theme"; exit 1
fi
for f in /usr/share/glib-2.0/schemas/*ilex*.override; do
    ! grep -q 'org.gnome.desktop' "$f" || { echo "$f restyles GNOME"; exit 1; }
done
"#,
    )?
    .ok("no global GTK, portal or GSettings theme default")?;
    println!("     no global GTK theme, portal or GSettings interface default");
    if !selection.shells.iter().any(|s| s == "kde") {
        return Ok(());
    }
    // The KDE shell's GTK pieces are all there.
    vm.ssh(
        r#"set -eu
grep -qx 'UseIn=KDE' /usr/share/xdg-desktop-portal/portals/kde.portal
grep -q 'org.freedesktop.impl.portal.Settings' /usr/share/xdg-desktop-portal/portals/kde.portal
grep -qx 'org.freedesktop.impl.portal.Settings=kde;gtk;' /usr/share/xdg-desktop-portal/kde-portals.conf
test -f /usr/share/themes/Breeze/gtk-3.0/gtk.css -a -f /usr/share/themes/Breeze/gtk-4.0/gtk.css
test -f /usr/lib64/qt6/plugins/kf6/kded/gtkconfig.so
test -f /usr/lib64/gtk-3.0/modules/libcolorreload-gtk-module.so
"#,
    )?
    .ok("KDE shell GTK integration files")?;
    // The Plasma-only defaults: the env script is sourced by startplasma only,
    // the autostart is KDE-only, and the dconf profile resolves and is empty.
    vm.ssh(
        r#"set -eu
grep -qx 'export DCONF_PROFILE=ilex-kde' /etc/xdg/plasma-workspace/env/50-ilex-kde.sh
test "$(cat /usr/share/dconf/profile/ilex-kde)" = user-db:kde
grep -qx 'OnlyShowIn=KDE;' /etc/xdg/autostart/ilex-kde-gtk-theme.desktop
grep -qx 'Exec=/usr/libexec/ilex/kde-gtk-theme' /etc/xdg/autostart/ilex-kde-gtk-theme.desktop
test -x /usr/libexec/ilex/kde-gtk-theme
sh -n /usr/libexec/ilex/kde-gtk-theme
out=$(DCONF_PROFILE=ilex-kde dconf read /org/gnome/desktop/interface/gtk-theme 2>&1 || true)
test -z "$out" || { echo "dconf profile ilex-kde: $out"; exit 1; }
"#,
    )?
    .ok("KDE shell Plasma-only defaults")?;
    println!("     KDE shell: GTK integration, KDE dconf profile and Plasma-only autostart present");
    // Everything the shell's overlay ships is scoped to Plasma.
    let overlay = common::workspace()?.root.join("shells/kde/rootfs");
    let mut seen = Vec::new();
    kde_overlay_paths(&overlay, &overlay, &mut seen)?;
    for path in &seen {
        let scoped = path.starts_with("etc/xdg/plasma-workspace/env/")
            || path == "etc/xdg/autostart/ilex-kde-gtk-theme.desktop"
            || path == "usr/libexec/ilex/kde-gtk-theme"
            || path == "usr/share/dconf/profile/ilex-kde";
        ensure(scoped, || format!("KDE overlay path {path} is not scoped to a Plasma session"))?;
    }
    println!("     KDE overlay: {} files, all Plasma-scoped", seen.len());
    Ok(())
}

fn kde_overlay_paths(root: &std::path::Path, directory: &std::path::Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            kde_overlay_paths(root, &path, out)?;
        } else {
            out.push(path.strip_prefix(root)?.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// systemd finished booting and nothing is degraded.
fn system_running(vm: &EphemeralVm) -> Result<()> {
    let state = vm
        .ssh_timeout(
            "systemctl is-system-running --wait",
            std::time::Duration::from_secs(300),
        )?
        .out()
        .to_string();
    if state == "running" {
        return Ok(());
    }
    let failed = vm.ssh("systemctl --failed --no-pager --plain")?.stdout;
    anyhow::bail!("system state is `{state}`, expected `running`\n{failed}")
}

/// No unit is in the failed state.
fn no_failed_units(vm: &EphemeralVm) -> Result<()> {
    vm.ssh_timeout(
        "systemctl is-system-running --wait",
        std::time::Duration::from_secs(300),
    )?;
    let failed = vm
        .ssh("systemctl --failed --no-legend --no-pager --plain")?
        .ok("systemctl --failed")?;
    ensure(failed.out().is_empty(), || {
        format!("failed units:\n{}", failed.out())
    })
}

/// sshd answers (it carried this very connection) and is the unit systemd
/// thinks it is.
fn sshd_active(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("systemctl is-active sshd.service")?
        .ok("systemctl is-active sshd.service")?;
    ensure(out.out() == "active", || {
        format!("sshd.service is `{}`", out.out())
    })
}

/// The binaries we ship check themselves from inside the guest.
fn agent_self_check(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("ilexctl system --self-check")?
        .ok("ilexctl system --self-check")?;
    ensure(out.stdout.contains("self-check: ok"), || {
        format!("unexpected output:\n{}", out.stdout)
    })
}

/// The VM runs the image under test.
///
/// An ephemeral VM boots the image's filesystem over virtiofs, with no ostree
/// deployment, so `bootc status` cannot name the image here (that assertion
/// lives in the `upgrade::` tier, on an installed system). Instead compare
/// the image bcvk started the VM from with the one under test, and a binary
/// the guest runs with the same file inside the image.
fn image_under_test(vm: &EphemeralVm) -> Result<()> {
    let want = bcvk::image_id(vm.image())?;
    let got = vm.booted_image_id()?;
    ensure(got == want, || {
        format!(
            "VM container runs image {got}, expected {want} ({})",
            vm.image()
        )
    })?;

    let in_image = bcvk::image_file_sha256(vm.image(), ILEXCTL)?;
    let in_guest = vm
        .ssh(&format!("sha256sum {ILEXCTL}"))?
        .ok("sha256sum in the guest")?;
    let in_guest = in_guest
        .out()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    ensure(in_guest == in_image, || {
        format!(
            "{ILEXCTL} is {in_guest} in the guest but {in_image} in {}",
            vm.image()
        )
    })
}

/// bootc works in the guest. With no deployment in an ephemeral VM there is
/// no booted image to compare, but if a future bcvk does report one it must be
/// the image under test.
fn bootc_status(vm: &EphemeralVm) -> Result<()> {
    let out = vm.ssh("bootc status --json")?.ok("bootc status --json")?;
    let status: Value =
        serde_json::from_str(&out.stdout).context("parsing `bootc status --json`")?;
    ensure(status["kind"] == "BootcHost", || {
        format!("not a BootcHost document:\n{}", out.stdout)
    })?;
    if let Some(booted) = status["status"]["booted"]["image"]["image"]["image"].as_str() {
        let want = vm.image();
        ensure(same_image(booted, want), || {
            format!("bootc status shows {booted}, expected {want}")
        })?;
    }
    Ok(())
}

/// The selected shells determine the target, also without a display attached.
fn graphical_default(vm: &EphemeralVm) -> Result<()> {
    let selection = common::selection()?;
    let out = vm
        .ssh("systemctl get-default")?
        .ok("systemctl get-default")?;
    ensure(out.out() == selection.default_target(), || {
        format!(
            "default target is {}, expected {}",
            out.out(),
            selection.default_target()
        )
    })
}

/// Compare the guest with both image labels and checkout manifests. The
/// agent's self-check separately verifies that the configuration works.
fn configuration(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("cat /etc/containers/policy.json")?
        .ok("reading the container signature policy")?;
    let policy: Value = serde_json::from_str(&out.stdout)?;
    ensure(
        policy["default"] == serde_json::json!([{ "type": "reject" }]),
        || "container policy default must reject for enforcing bootc pulls".into(),
    )?;
    ensure(
        policy["transports"]["docker"]["ghcr.io/reidond/ilex"]
            == serde_json::json!([{
                "type": "sigstoreSigned",
                "keyPath": "/etc/pki/containers/ilex.pub",
                "signedIdentity": { "type": "matchRepository" }
            }]),
        || "ilex must require its sigstore signature and matching repository identity".into(),
    )?;
    for transport in [
        "atomic",
        "docker",
        "containers-storage",
        "dir",
        "docker-archive",
        "docker-daemon",
        "oci",
        "oci-archive",
        "sif",
        "tarball",
    ] {
        ensure(
            policy["transports"][transport][""]
                == serde_json::json!([{ "type": "insecureAcceptAnything" }]),
            || format!("container policy must explicitly accept the {transport} transport"),
        )?;
    }
    let selection = common::selection()?;
    let out = vm
        .ssh("ilexctl system config --json")?
        .ok("ilexctl system config --json")?;
    let guest: Value = serde_json::from_str(&out.stdout)?;
    let image = &guest["image"];
    let names = |key: &str| -> Result<Vec<String>> {
        image[key]
            .as_array()
            .context("image configuration list missing")?
            .iter()
            .map(|entry| {
                entry["name"]
                    .as_str()
                    .map(str::to_string)
                    .context("configuration name missing")
            })
            .collect()
    };
    ensure(
        image["schema"] == 1 && guest["machine"]["schema"] == 1,
        || "configuration schema must be 1".into(),
    )?;
    ensure(names("shells")? == selection.shells, || {
        "shells differ from image labels".into()
    })?;
    let labels = ilex_devkit::cmd::output(std::process::Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        vm.image(),
    ]))?;
    let labels: Value = serde_json::from_str(&labels)?;
    for key in ["design-systems", "default-design-system"] {
        ensure(image.get(key).is_none(), || {
            format!("image.json must not contain {key}")
        })?;
        ensure(labels.get(format!("ilex.{key}")).is_none(), || {
            format!("image must not have an ilex.{key} label")
        })?;
    }
    vm.ssh(
        "test ! -e /usr/share/ilex/design-systems && \
         test ! -L /usr/share/ilex/design-systems",
    )?
    .ok("design system directory must not be installed")?;
    println!("     design system directory, metadata and labels absent");
    ensure(
        image["default-shell"] == serde_json::to_value(&selection.default_shell)?,
        || "default-shell differs from image label".into(),
    )?;
    ensure(
        guest["machine"]["name"].as_str() == Some(&selection.machine),
        || "machine differs from image label".into(),
    )?;
    for (key, path, selected) in [
        ("packages", "packages.toml", true),
        ("shell-packages", "shells/packages.toml", !selection.shells.is_empty()),
    ] {
        let mut expected = serde_json::json!({"install": [], "exclude": []});
        if selected {
            let manifest = ilex_devkit::config::manifest(&common::workspace()?.root.join(path))?;
            for kind in ["install", "exclude"] {
                let mut entries = Vec::new();
                for group in manifest.values() {
                    if let Some(values) = group.get(kind) {
                        entries.extend(values.as_array().context("package list missing")?
                            .iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>, _>>()?);
                    }
                }
                expected[kind] = Value::Array(entries);
            }
        }
        // TOML host tables sort keys; the composer preserves file order.
        // Compare multisets so table ordering cannot hide missing/extra entries.
        for kind in ["install", "exclude"] {
            let entries = |value: &Value| -> Result<Vec<String>> {
                let mut entries = value[kind].as_array().context("package list missing")?
                    .iter().map(|v| v.as_str().map(str::to_owned).context("package name missing"))
                    .collect::<Result<Vec<_>>>()?;
                entries.sort();
                Ok(entries)
            };
            ensure(entries(&image[key])? == entries(&expected)?, || {
                format!("{key}.{kind} differs from {path}")
            })?;
        }
        let packages = toml::Table::from_iter([("packages".into(), toml::Value::try_from(&expected)?)]);
        check_packages(vm, &packages)?;
        if key == "shell-packages" && selected {
            for package in expected["exclude"].as_array().context("shell excludes missing")? {
                let name = package.as_str().context("shell exclude must be a string")?;
                if !name.contains(['*', '?', '[', ']']) {
                    vm.ssh(&format!("rpm -q -- {}; result=$?; test \"$result\" -eq 1", common::quote(name)))?
                        .ok(&format!("literal shell exclude {name} must not be installed"))?;
                    println!("     literal shell exclude {name} absent");
                }
            }
        }
    }
    if selection.shells.is_empty() {
        // A machine may legitimately add desktop packages to the bare base.
        if selection.machine == "generic" {
            vm.ssh(
                "test -z \"$(find /usr/share/wayland-sessions /usr/share/xsessions -mindepth 1 -maxdepth 1 2>/dev/null)\" && \
                 test ! -e /etc/systemd/system/display-manager.service && \
                 test ! -L /etc/systemd/system/display-manager.service && \
                 test ! -e /usr/bin/firefox && test -x /usr/bin/flatpak"
            )?.ok("bare base has no desktop package files")?;
            println!("     bare base: sessions, display manager and desktop apps absent");
        }
        vm.ssh(
            "test ! -L /etc/systemd/user/default.target.wants/ilex-update-notify.path && \
             test ! -L /etc/systemd/user/graphical-session.target.wants/ilex-update-notify.service"
        )?.ok("shell-free image has no enabled notifier")?;
        println!("     no shell: notifier enablement absent");
    } else {
        vm.ssh(
            "test -L /etc/systemd/user/default.target.wants/ilex-update-notify.path && \
             test -L /etc/systemd/user/graphical-session.target.wants/ilex-update-notify.service"
        )?.ok("shell image has both notifier enablement links")?;
        println!("     shell image: both notifier enablement links present");
    }
    for (index, name) in selection.shells.iter().enumerate() {
        let manifest = common::manifest("shells", name, "shell.toml")?;
        let session = common::shell_property(&manifest, "session", "desktop")?;
        let compositor = common::shell_property(&manifest, "session", "compositor")?;
        vm.ssh(&format!(
            "test -f {} && test -x {}",
            common::quote(&format!("/usr/share/wayland-sessions/{session}.desktop")),
            common::quote(&format!("/usr/bin/{compositor}"))
        ))?
        .ok("shell session and compositor")?;
        // Compare all declared manifest fields, including packages.
        let declared = app_manifest(&manifest)?;
        for (key, value) in declared
            .as_object()
            .context("shell manifest is not an object")?
        {
            ensure(image["shells"][index][key] == *value, || {
                format!("shell {name}: `{key}` differs from the checkout")
            })?;
        }
        check_packages(vm, &manifest)?;
        // Only the default shell's greeter is enabled: with several shells each
        // package enables its own, and the Containerfile leaves the default's.
        if Some(name.as_str()) != selection.default_shell.as_deref()
            && let Ok(unit) = common::shell_property(&manifest, "greeter", "unit")
        {
            let state = vm.ssh(&format!("systemctl is-enabled -- {}", common::quote(&unit)))?;
            ensure(state.out() != "enabled", || {
                format!("shell {name}: greeter {unit} must not be enabled beside the default shell's")
            })?;
            println!("     shell {name}: greeter {unit} is {}", state.out());
        }
        if let Some(excludes) = manifest.get("packages").and_then(|a| a.get("exclude")).and_then(|a| a.as_array()) {
            for package in excludes {
                let name = package.as_str().context("exclude must be a string")?;
                if !name.contains(['*', '?', '[', ']']) {
                    vm.ssh(&format!("rpm -q -- {}; test $? -eq 1", common::quote(name)))?.ok("excluded shell RPM absent")?;
                }
            }
        }
        let overlay = common::workspace()?
            .root
            .join("shells")
            .join(name)
            .join("rootfs");
        if overlay.is_dir() {
            check_overlay(vm, &overlay, &overlay)?;
        }
    }
    ensure(guest["machine"].get("publish").is_none(), || {
        "machine.json must not contain publish; publications belong in image.toml".into()
    })?;
    ensure(guest["machine"].get("match").is_none(), || {
        "machine.json must not contain installer-only match metadata".into()
    })?;
    let machine = common::manifest("machines", &selection.machine, "machine.toml")?;
    let declared = if selection.machine == "generic" {
        serde_json::to_value(&machine)?
    } else {
        app_manifest(&machine)?
    };
    for (key, value) in declared
        .as_object()
        .context("machine manifest is not an object")?
    {
        if key == "match" { continue; }
        ensure(guest["machine"][key] == *value, || {
            format!(
                "machine {}: `{key}` differs from the checkout",
                selection.machine
            )
        })?;
    }
    check_packages(vm, &machine)?;
    let overlay = common::workspace()?.root.join("machines").join(&selection.machine).join("rootfs");
    if overlay.is_dir() { check_overlay(vm, &overlay, &overlay)?; }
    check_unselected_configurations(vm, &selection)?;
    println!("     configuration matches labels and checkout: {selection:?}");
    Ok(())
}

/// Sessions, compositor commands and greeter units identify an omitted shell.
/// Its dependencies may legitimately come from common/base packages or a
/// selected shell's group; neither package names nor group members identify it.
fn check_unselected_configurations(
    vm: &EphemeralVm,
    selection: &ilex_devkit::config::Selection,
) -> Result<()> {
    use std::collections::BTreeSet;

    use ilex_devkit::config;

    let ws = common::workspace()?;
    let mut sessions = BTreeSet::new();
    let mut compositors = BTreeSet::new();
    let mut greeters = BTreeSet::new();
    let mut shared_overlays = vec![ws.root.join("rootfs")];
    shared_overlays.push(ws.root.join("machines").join(&selection.machine).join("rootfs"));
    for name in &selection.shells {
        let shell = common::manifest("shells", name, "shell.toml")?;
        shared_overlays.push(ws.root.join("shells").join(name).join("rootfs"));
        sessions.insert(common::shell_property(&shell, "session", "desktop")?);
        compositors.insert(common::shell_property(&shell, "session", "compositor")?);
        if shell.contains_key("greeter") {
            greeters.insert(common::shell_property(&shell, "greeter", "unit")?);
        }
    }
    for name in config::available(&ws.root, "shells")? {
        if selection.shells.contains(&name) {
            continue;
        }
        let shell = common::manifest("shells", &name, "shell.toml")?;
        let desktop = common::shell_property(&shell, "session", "desktop")?;
        let compositor = common::shell_property(&shell, "session", "compositor")?;
        for (kind, path, shared) in [
            ("session", format!("/usr/share/wayland-sessions/{desktop}.desktop"), sessions.contains(&desktop)),
            ("compositor", format!("/usr/bin/{compositor}"), compositors.contains(&compositor)),
        ] {
            if shared {
                continue;
            }
            let path = common::quote(&path);
            vm.ssh(&format!(
                "test ! -e {path} && test ! -L {path}"
            ))?
            .ok(&format!("unselected shell {name}: {kind} {path} must be absent"))?;
            println!("     unselected shell {name}: {kind} {path} absent");
        }
        if shell.contains_key("greeter") {
            let unit = common::shell_property(&shell, "greeter", "unit")?;
            if !greeters.contains(&unit) {
                let out = vm.ssh(&format!("systemctl cat -- {}", common::quote(&unit)))?;
                ensure(out.code == Some(1), || {
                    format!("unselected shell {name}: greeter {unit} must not be installed\n{}\n{}", out.stdout, out.stderr)
                })?;
                println!("     unselected shell {name}: greeter {unit} not installed");
            }
        }
        let overlay = ws.root.join("shells").join(&name).join("rootfs");
        if overlay.is_dir() {
            check_overlay_absent(vm, &format!("shell {name}"), &overlay, &overlay, &shared_overlays)?;
        }
    }
    for name in config::available(&ws.root, "machines")? {
        if name == selection.machine {
            continue;
        }
        let overlay = ws.root.join("machines").join(&name).join("rootfs");
        if overlay.is_dir() {
            check_overlay_absent(vm, &format!("machine {name}"), &overlay, &overlay, &shared_overlays)?;
        }
    }
    Ok(())
}

/// Shared parent directories do not exempt their children: only an exact
/// source path in common rootfs or a selected shell/machine excuses an omitted leaf.
fn check_overlay_absent(
    vm: &EphemeralVm,
    configuration: &str,
    root: &std::path::Path,
    directory: &std::path::Path,
    shared: &[std::path::PathBuf],
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            check_overlay_absent(vm, configuration, root, &path, shared)?;
            continue;
        }
        let relative = path.strip_prefix(root)?;
        if shared.iter().any(|source| source.join(relative).symlink_metadata().is_ok()) {
            continue;
        }
        let destination = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
        let quoted = common::quote(&destination);
        vm.ssh(&format!("test ! -e {quoted} && test ! -L {quoted}"))?
            .ok(&format!("unselected {configuration}: overlay {destination} must be absent"))?;
        println!("     unselected {configuration}: overlay {destination} absent");
    }
    Ok(())
}

fn app_manifest(manifest: &toml::Table) -> Result<Value> {
    let mut value = serde_json::to_value(manifest)?;
    let apps = value.as_object_mut().context("manifest must be a table")?
        .entry("apps").or_insert_with(|| serde_json::json!({}));
    for key in ["flatpak", "rpm"] {
        apps.as_object_mut().context("apps must be a table")?
            .entry(key).or_insert_with(|| serde_json::json!([]));
    }
    Ok(value)
}

fn check_packages(vm: &EphemeralVm, manifest: &toml::Table) -> Result<()> {
    if let Some(packages) = manifest
        .get("apps")
        .and_then(|a| a.get("rpm"))
        .and_then(|a| a.as_array())
    {
        for package in packages {
            let name = package.as_str().context("app RPM must be a string")?;
            if !name.starts_with('@') {
                vm.ssh(&format!("rpm -q -- {}", common::quote(name)))?
                    .ok("app RPM installed")?;
            }
        }
    }
    if let Some(packages) = manifest
        .get("packages")
        .and_then(|p| p.get("install"))
        .and_then(|v| v.as_array())
    {
        for package in packages {
            let name = package.as_str().context("package must be a string")?;
            if !name.starts_with('@') {
                vm.ssh(&format!("rpm -q -- {}", common::quote(name)))?
                    .ok("manifest package installed")?;
            }
        }
    }
    Ok(())
}

/// Every overlay file must survive unchanged: the installer rejects collisions.
fn check_overlay(
    vm: &EphemeralVm,
    root: &std::path::Path,
    directory: &std::path::Path,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let destination = format!(
            "/{}",
            path.strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/")
        );
        if path.is_dir() && !path.is_symlink() {
            check_overlay(vm, root, &path)?;
        } else if path.is_symlink() {
            let target = std::fs::read_link(&path)?.to_string_lossy().into_owned();
            let guest = vm
                .ssh(&format!("readlink -- {}", common::quote(&destination)))?
                .ok("overlay symlink")?;
            ensure(guest.out() == target, || {
                format!("overlay symlink {destination} differs")
            })?;
        } else {
            check_file(vm, &path, &destination)?;
        }
    }
    Ok(())
}

fn check_file(vm: &EphemeralVm, source: &std::path::Path, destination: &str) -> Result<()> {
    let host = ilex_devkit::cmd::output(
        std::process::Command::new("sha256sum")
            .arg("--")
            .arg(source),
    )?;
    let guest = vm
        .ssh(&format!("sha256sum -- {}", common::quote(destination)))?
        .ok("configuration file sha256")?;
    ensure(
        host.split_whitespace().next() == guest.out().split_whitespace().next(),
        || format!("{destination} sha256 differs from {}", source.display()),
    )
}

/// Compare image references, ignoring a leading `localhost/`.
fn same_image(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.strip_prefix("localhost/").unwrap_or(s).to_string();
    norm(a) == norm(b)
}

/// Signature enforcement does nothing here: an ephemeral VM has no booted
/// deployment, so `ilexctl system enforce-sigpolicy` (which `ilex-sigpolicy.service`
/// runs at every boot of an installed machine) must neither fail nor try to
/// switch anything. The unit itself must not be in the failed state, which
/// `no_failed_units` already requires; the real switch is tested on an
/// installed system, in `upgrade::sigpolicy_enforced`.
fn sigpolicy_noop(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("ilexctl system enforce-sigpolicy")?
        .ok("ilexctl system enforce-sigpolicy")?;
    ensure(out.stdout.contains("nothing to do"), || {
        format!("expected a no-op, got:\n{}{}", out.stdout, out.stderr)
    })?;
    let unit = vm.ssh("systemctl is-failed ilex-sigpolicy.service")?;
    ensure(unit.out() != "failed", || {
        "ilex-sigpolicy.service failed".to_string()
    })
}
