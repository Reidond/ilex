//! Helpers shared by every tier.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Result, bail};
use ilex_devkit::Workspace;
use libtest_mimic::{Failed, Trial};

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
