//! ilex end-to-end tests. There are no unit tests: every test boots the
//! image in QEMU and asserts over SSH.
//!
//! Tiers are selected with the standard libtest filter, e.g.
//! `cargo xtask test ephemeral::`. The image under test comes in through
//! `ILEX_IMAGE` (default: this checkout's `localhost/ilex:<tag>`).

mod common;
mod disk;
mod ephemeral;
mod graphical;
mod upgrade;

fn main() {
    let args = libtest_mimic::Arguments::from_args();
    let mut trials = Vec::new();
    trials.extend(ephemeral::trials());
    trials.extend(disk::trials());
    trials.extend(upgrade::trials());
    trials.extend(graphical::trials());
    libtest_mimic::run(&args, trials).exit();
}
