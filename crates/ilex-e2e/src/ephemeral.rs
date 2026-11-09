//! `ephemeral::` tier: The image booted directly as an ephemeral VM, no disk.

use libtest_mimic::Trial;

pub fn trials() -> Vec<Trial> {
    Vec::new()
}
