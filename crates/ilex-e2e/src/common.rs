//! Helpers shared by every tier.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail};
use ilex_devkit::{
    Workspace, cmd,
    config::{self, Selection},
};
use libtest_mimic::{Failed, Trial};

/// Installed guests must use the image's narrow vendor mask, not a fixture
/// kernel argument or runtime mask. Read mountinfo without triggering autofs.
pub const BOOT_MOUNT_CHECK: &str = r#"set -eu
test -f /usr/lib/systemd/system/boot.automount
test ! -s /usr/lib/systemd/system/boot.automount
test ! -e /run/systemd/system/boot.automount
test ! -L /run/systemd/system/boot.automount
test -x /usr/lib/systemd/system-generators/systemd-gpt-auto-generator
test "$(systemctl show boot.automount -p LoadState --value)" = masked
test "$(systemctl show boot.mount -p ActiveState --value)" = active
awk '$5 == "/boot" { count++; for (i=6; i<NF; i++) if ($i == "-" && $(i+1) == "autofs") bad=1 } END { exit (!count || bad) }' /proc/1/mountinfo
for arg in $(cat /proc/cmdline); do
    case "$arg" in systemd.gpt_auto=0|systemd.gpt_auto=no|systemd.gpt_auto=off|systemd.gpt_auto=false|rd.systemd.gpt_auto=0|rd.systemd.gpt_auto=no|rd.systemd.gpt_auto=off|rd.systemd.gpt_auto=false) exit 1;; esac
done
echo 'shipped boot.automount mask verified; /boot mounted without autofs; GPT discovery enabled'
"#;

/// The checkout under test.
pub fn workspace() -> Result<Workspace> {
    Workspace::discover()
}

/// Image under test: `ILEX_IMAGE`, or this checkout's local image.
pub fn image() -> Result<String> {
    match std::env::var("ILEX_IMAGE") {
        Ok(image) if !image.is_empty() => Ok(image),
        _ => Ok(workspace()?.image_ref()),
    }
}

/// Expectations come from the image actually being tested, including when
/// ILEX_IMAGE selects a candidate or a later machine layer.
pub fn selection() -> Result<Selection> {
    let text = cmd::output(std::process::Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        &image()?,
    ]))?;
    let labels = serde_json::from_str::<Option<_>>(&text).context("parsing image labels")?;
    Selection::from_labels(&labels.unwrap_or_default())
}

pub fn default_shell() -> Result<Option<toml::Table>> {
    selection()?.default_shell_manifest(&workspace()?.root)
}

pub fn shell_property(shell: &toml::Table, table: &str, key: &str) -> Result<String> {
    shell
        .get(table)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .with_context(|| format!("shell manifest lacks `{table}.{key}`"))
}

pub fn greeter() -> Result<Option<String>> {
    default_shell()?
        .map(|shell| shell_property(&shell, "greeter", "unit"))
        .transpose()
}

pub fn manifest(directory: &str, name: &str, file: &str) -> Result<toml::Table> {
    config::validate_name(name)?;
    config::manifest(&workspace()?.root.join(directory).join(name).join(file))
}

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A test whose body returns an `anyhow::Result`; failures print the whole
/// error chain.
pub fn trial(name: &str, body: impl FnOnce() -> Result<()> + Send + 'static) -> Trial {
    Trial::test(name, move || {
        body().map_err(|e| Failed::from(format!("{e:#}")))
    })
}

/// `Ok(())` when `cond` holds, otherwise an error built from `msg`.
pub fn ensure(cond: bool, msg: impl FnOnce() -> String) -> Result<()> {
    if cond { Ok(()) } else { bail!("{}", msg()) }
}

/// A VM shared by the tests of one tier ("one VM per test group, not per
/// test").
///
/// The first test that needs it boots it; the others wait for the boot and
/// then share it. The VM is released as soon as the last test that was going
/// to use it has finished (so its VM slot is free for the next tier), and in
/// any case by [`Group::finish`], which `main` calls after the run. Because
/// release happens through `Drop`, a panicking test cannot leak the VM.
pub struct Group<V> {
    name: &'static str,
    boot: fn() -> Result<V>,
    state: Mutex<State<V>>,
    /// Tests still to run in this group; `usize::MAX` until [`Group::expect`].
    remaining: AtomicUsize,
}

enum State<V> {
    Idle,
    Up(Arc<V>),
    /// Booting failed; every test of the group reports the same error.
    Failed(String),
    Released,
}

impl<V> Group<V> {
    pub const fn new(name: &'static str, boot: fn() -> Result<V>) -> Self {
        Self {
            name,
            boot,
            state: Mutex::new(State::Idle),
            remaining: AtomicUsize::new(usize::MAX),
        }
    }

    /// Declare how many tests of this group will run, so the last one to
    /// finish can release the VM.
    pub fn expect(&self, tests: usize) {
        self.remaining.store(tests, Ordering::SeqCst);
    }

    /// Run `body` against the group's VM, booting it first if needed.
    pub fn run<R>(&self, body: impl FnOnce(&V) -> Result<R>) -> Result<R> {
        let _finished = Finished(self);
        let vm = {
            let mut state = self.lock();
            match &*state {
                State::Up(vm) => Arc::clone(vm),
                State::Failed(why) => bail!("{} VM failed to boot: {why}", self.name),
                State::Released => bail!("{} VM was already released", self.name),
                State::Idle => {
                    eprintln!("ilex: booting the {} VM", self.name);
                    match (self.boot)() {
                        Ok(vm) => {
                            let vm = Arc::new(vm);
                            *state = State::Up(Arc::clone(&vm));
                            vm
                        }
                        Err(e) => {
                            let why = format!("{e:#}");
                            *state = State::Failed(why.clone());
                            bail!("{} VM failed to boot: {why}", self.name);
                        }
                    }
                }
            }
        };
        body(&vm)
    }

    /// Release the VM now, whatever is left to run.
    pub fn finish(&self) {
        let released = std::mem::replace(&mut *self.lock(), State::Released);
        drop(released);
    }

    fn lock(&self) -> MutexGuard<'_, State<V>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Decrements the group's counter when a test ends, however it ends.
struct Finished<'a, V>(&'a Group<V>);

impl<V> Drop for Finished<'_, V> {
    fn drop(&mut self) {
        if self.0.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.finish();
        }
    }
}
