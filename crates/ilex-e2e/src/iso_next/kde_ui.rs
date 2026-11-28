//! The KDE edition's interface on its real ISO, by real key input over QMP,
//! observed through the same serial markers as the GNOME edition's and through
//! screenshots. Secure Boot on KVM, KWin as the session's compositor, the real
//! session unit and the real Kirigami window.
//!
//! The helper is **simulated** (the fixture scenarios of `ilex-installer-flow`,
//! asked for with `ilex.installer.simulate=<scenario>` typed into GRUB's
//! editor, which a boot of a released ISO is not talked into): the KDE
//! edition's catalogue has no composition of its own until the KDE shell is
//! published (`generic.kde`, PR #85; the helper refuses to start for an edition
//! the catalogue lacks: "edition kde has no generic composition"), so the real
//! helper has nothing to plan or install for it yet, and the installation
//! pages' trials that need it (a checked plan against the registry, a real
//! installation) belong to the release PR (K3). What these trials show is that
//! the Kirigami pages work on the real ISO under KWin: the keyboard alone moves
//! through them, the flow's markers come in the same order and words, an erase
//! needs its confirmation, a failure ends on the problem page and its log can
//! be saved, and nothing typed in a password field is printed or logged.
//!
//! * `iso::next_kde_welcome`: the welcome page; the layout search dialog by
//!   keyboard; German is applied in the running KWin and its keymap is proved by
//!   the test field (`yz` types `zy`), then US (`yz` stays `yz`) and German
//!   again; then German (no dead keys) to the plain German layout and back,
//!   which have the same layout code, told apart by a dead key (the key at
//!   US `` ` `` types `^` or composes a circumflex); Enter continues to the
//!   machine page.
//! * `iso::next_kde_detect_board` / `_detect_generic`: the machine page for a
//!   detected board and for a computer nothing matches.
//! * `iso::next_kde_kernel_argument` / `_unknown`: the boot override's notice.
//! * `iso::next_kde_install`: every page by key to the done page, the erase
//!   dialog cancelled once and then confirmed; the password in neither the
//!   serial console nor the journal.
//! * `iso::next_kde_problem`: a failed image stage ends on the problem page;
//!   Save the log by keyboard.
//! * `iso::next_kde_frozen_back`: Alt+Left (Kirigami's own Back shortcut) while
//!   the plan is checked does not leave the window on another page.
//! * `iso::next_kde_encryption`: the recovery key page by keyboard (simulated
//!   helper, so the page's own behaviour is what is looked at): a window that
//!   was not given the key shows its field, a wrongly typed-back key is refused
//!   (the emptied field is in the screenshot, not asserted), and Tab and Enter
//!   take the way out. The passphrase is walked by `iso::next_kde_install`; the
//!   chip, the right key and the done page by the two real installations below.
//!
//! And the same two installations as the GNOME edition's keyboard trials, with
//! the **real helper** installing the edition's composition (the registry's
//! `generic.kde`, which a main run of CI publishes), on KVM with SMM: Secure
//! Boot on and a persistent TPM, the way `iso::next_security` boots, with the
//! person at the Kirigami interface instead of a script:
//!
//! * `iso::next_kde_security`: an encrypted installation by keys, the recovery
//!   key confirmed on its page and the security chip enrolled. The installed
//!   disk is started cold with nobody at the keyboard and the chip opens it;
//!   on another TPM the recovery key typed at the prompt opens it. Everything
//!   `iso::next_security` asserts of the volume, the records and the secrets
//!   holds (the same code asserts it).
//! * `iso::next_kde_security_declined`: the same, where the person cannot
//!   confirm the key and goes on without one with the page's last button: the
//!   disk opens with its passphrase, the chip is not enrolled.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;

use super::install_ui::{
    confirm_erase, count, helper_state, is_event, is_page, no_password, nth_marker, wait_done,
};
use super::security::{self, Edit, Machine, Variant};
use super::ui::{
    Booted, EVENT_TIMEOUT, PASSWORD, boot_of, wait_event, wait_page_marker, welcome_up_of,
};
use super::{
    Edition, GRUB_COUNTDOWN, PAGE_TIMEOUT, console, diagnostics, field, trial, wait_for_text,
    wait_page,
};
use crate::common;
use crate::installer::apply;
use crate::installer::apply::encrypted::US;
use crate::iso;
use crate::selinux;
use ilex_devkit::installer;

const KDE: Edition = Edition::Kde;
/// The fixed public test credential (SMBIOS) that lets the interface print the
/// text of the layout test field.
const ECHO_CREDENTIAL: &str = "ilex.installer.echo-test";
/// The administrator's name the install trial types.
const ADMIN: &str = "ilexmaint";
/// The disk's passphrase the trials type: spaces, quotes, a colon and an
/// ampersand, as the GNOME edition's trials use.
const PASSPHRASE: &str = US.keys;

pub(super) fn trials() -> Vec<Trial> {
    vec![
        trial("iso::next_kde_welcome", welcome),
        trial("iso::next_kde_detect_board", || {
            detect("iso-next-kde-board", "ms-7c35-board", true, |machine| {
                ensure!(
                    field(machine, "selected")? == "ms-7c35.gnome"
                        && field(machine, "origin")? == "detected",
                    "the machine page for the detected board: {machine:?}"
                );
                Ok(())
            })
        }),
        trial("iso::next_kde_detect_generic", || {
            detect("iso-next-kde-generic", "generic", false, |machine| {
                ensure!(
                    field(machine, "selected")? == "generic.gnome"
                        && field(machine, "origin")? == "not-recognised",
                    "the machine page for a computer nothing matches: {machine:?}"
                );
                Ok(())
            })
        }),
        trial("iso::next_kde_kernel_argument", || {
            detect("iso-next-kde-karg", "boot-override", false, |machine| {
                ensure!(
                    field(machine, "origin")? == "chosen",
                    "the machine page for a boot override: {machine:?}"
                );
                Ok(())
            })
        }),
        trial("iso::next_kde_kernel_argument_unknown", || {
            detect("iso-next-kde-karg-bad", "bad-override", false, |machine| {
                ensure!(
                    field(machine, "origin")? == "bad-override",
                    "the machine page for an unknown boot override: {machine:?}"
                );
                Ok(())
            })
        }),
        trial("iso::next_kde_install", install),
        trial("iso::next_kde_encryption", encryption),
        trial("iso::next_kde_problem", problem),
        trial("iso::next_kde_frozen_back", frozen_back),
        trial("iso::next_kde_security", || security(Variant::Confirmed)),
        trial("iso::next_kde_security_declined", || {
            security(Variant::Declined)
        }),
    ]
}

/// Boot the KDE ISO with the simulated helper's `scenario` (typed into GRUB's
/// editor) and the optional SMBIOS strings.
fn boot(purpose: &'static str, scenario: &str, smbios: Vec<String>) -> Result<Booted> {
    let mut booted = boot_of(KDE, purpose, true, smbios, "1G")?;
    let artifacts = booted.artifacts.clone();
    let (iso, work) = (booted.iso.clone(), booted.work.clone());
    let vm = booted.guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    // The countdown is the point at which GRUB starts accepting edit keys.
    wait_for_text(vm, &serial, GRUB_COUNTDOWN, PAGE_TIMEOUT)?;
    iso::edit_kernel_line(
        &iso,
        &work,
        &format!(" ilex.installer.simulate={scenario}"),
        &ctl,
        &artifacts,
        &format!("next-{purpose}-grub-edit.png"),
        vm,
        &serial,
    )?;
    Ok(booted)
}

/// Run `observe` on a booted VM, with the diagnostics on failure and the
/// SELinux report (enforcing, no denial) at the end.
fn run(
    mut booted: Booted,
    name: &str,
    observe: impl FnOnce(&mut ilex_devkit::qemu::Vm, &Path) -> Result<()>,
) -> Result<()> {
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe(vm, &artifacts)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, &format!("next-{name}")));
    if outcome.is_err() {
        diagnostics(vm, &artifacts, name);
    }
    outcome
}

fn welcome() -> Result<()> {
    let echo = vec![format!(
        "type=11,value=io.systemd.credential:{ECHO_CREDENTIAL}=1"
    )];
    let booted = boot("iso-next-kde-welcome", "generic", echo)?;
    run(booted, "kde-welcome", |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        welcome_up_of(KDE, vm, &ctl, &serial, artifacts, "kde-welcome")?;
        ctl.save_screenshot(&artifacts.join("next-kde-welcome.png"))?;

        // The first control has the focus: Tab reaches the layout button, Enter
        // opens the search dialog (several hundred layouts are searched, not
        // scrolled), typing narrows it, Enter chooses the first match.
        ctl.key("Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-layouts.png"))?;
        ctl.type_text("german (no dead")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-layouts-search.png"))?;
        ctl.key("Return")?;
        let chosen = wait_event(vm, &serial, "layout", EVENT_TIMEOUT, |w| {
            w.get("layout").is_some_and(|l| l == "de-nodeadkeys")
        })?;
        // KWin takes the layout in the running session: the installer rewrote
        // `kxkbrc`, sent the KConfig change KWin listens for, and KWin said it
        // has the layout (`kde/keyboard.rs`).
        ensure!(
            field(&chosen, "applied")? == "true",
            "KWin did not take the German layout in the running session: {chosen:?}"
        );
        std::thread::sleep(Duration::from_secs(2));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-german.png"))?;

        // The layout is really in KWin's keymap: the test field takes the keys
        // `y` and `z` of a US keyboard as the German `z` and `y`. (The dialog
        // has closed and the layout button has the focus again.)
        ctl.key("Tab")?;
        ctl.type_text("yz")?;
        let german = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
            w.get("text").is_some_and(|t| t == "zy")
        })?;
        ensure!(
            field(&german, "layout")? == "de-nodeadkeys",
            "the test field reports {german:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-german-typed.png"))?;

        // The layout can be changed again (a second reconfiguration): back to
        // the US layout, where `yz` stays `yz`. The test field's text is
        // selected and replaced.
        ctl.key("shift+Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        // German is active: the parenthesis of a US keyboard's Shift+9 is Shift+8
        // here (everything else in the name is the same key on both).
        ctl.type_text("english ")?;
        ctl.key("shift+8")?;
        ctl.type_text("us")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        let back = wait_event(vm, &serial, "layout", EVENT_TIMEOUT, |w| {
            w.get("layout").is_some_and(|l| l == "us")
        })?;
        ensure!(
            field(&back, "applied")? == "true",
            "KWin did not take the US layout back: {back:?}"
        );
        std::thread::sleep(Duration::from_secs(3));
        ctl.key("Tab")?;
        ctl.key("ctrl+a")?;
        ctl.type_text("yz")?;
        let tried = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
            w.get("text").is_some_and(|t| t == "yz")
        })?;
        ensure!(
            field(&tried, "layout")? == "us",
            "the test field reports {tried:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-us.png"))?;

        // German again, and Continue is enabled with it: Enter in the field
        // goes on to the machine page.
        ctl.key("shift+Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.type_text("german (no dead")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        let again = nth_marker(
            vm,
            &serial,
            "German again",
            2,
            EVENT_TIMEOUT,
            is_event("layout", "layout", "de-nodeadkeys"),
        )?;
        ensure!(
            field(&again, "applied")? == "true",
            "KWin did not take German (no dead keys) again: {again:?}"
        );
        std::thread::sleep(Duration::from_secs(3));

        // A variant and its plain layout have the same code (`de`): KWin's
        // answer says which one it has by its description only. The key under
        // Esc on a US keyboard (`) is `^` on German (no dead keys) and the dead
        // circumflex on German: the first types `^e`, the second composes `ô`
        // from `o`.
        ctl.key("Tab")?;
        ctl.key("ctrl+a")?;
        ctl.type_text("`e")?;
        let variant = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
            w.get("text").is_some_and(|t| t == "^e")
        })?;
        ensure!(
            field(&variant, "layout")? == "de-nodeadkeys",
            "the test field reports {variant:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-german-dead-key-off.png"))?;

        // German (no dead keys) to German: `applied=true` comes only after
        // KWin says it has the plain layout, so the keys typed right after it
        // already make a dead key (a poll that took the old layout's code `de`
        // for the new one would answer too early).
        ctl.key("shift+Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.type_text("german")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-layouts-plain.png"))?;
        ctl.key("Return")?;
        let plain = wait_event(vm, &serial, "layout", EVENT_TIMEOUT, |w| {
            w.get("layout").is_some_and(|l| l == "de")
        })?;
        ensure!(
            field(&plain, "applied")? == "true",
            "KWin did not take the plain German layout: {plain:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
        ctl.key("Tab")?;
        ctl.key("ctrl+a")?;
        ctl.type_text("`o")?;
        let composed = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
            // (The field still holds the earlier `^e` when the layout changes.)
            w.get("layout").is_some_and(|l| l == "de")
                && w.get("text").is_some_and(|t| !t.is_empty() && t != "^e")
        })?;
        ensure!(
            field(&composed, "text")? == "ô",
            "plain German composes a circumflex, it does not type `^`: {composed:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-german-dead-key-on.png"))?;

        // And back to the variant: the plain layout's code `de` must not stand
        // for it either.
        ctl.key("shift+Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        // German is active: the parenthesis is Shift+8 here.
        ctl.type_text("german ")?;
        ctl.key("shift+8")?;
        ctl.type_text("no dead")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        let back = nth_marker(
            vm,
            &serial,
            "German (no dead keys) after German",
            3,
            EVENT_TIMEOUT,
            is_event("layout", "layout", "de-nodeadkeys"),
        )?;
        ensure!(
            field(&back, "applied")? == "true",
            "KWin did not take German (no dead keys) after German: {back:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
        ctl.key("Tab")?;
        ctl.key("ctrl+a")?;
        ctl.type_text("`u")?;
        let literal = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
            w.get("text").is_some_and(|t| t == "^u")
        })?;
        ensure!(
            field(&literal, "layout")? == "de-nodeadkeys",
            "the test field reports {literal:?}"
        );

        // What each layout typed, for the record.
        std::fs::write(
            artifacts.join("next-kde-welcome-dead-keys.txt"),
            format!(
                "German (no dead keys): {}\nGerman: {}\nGerman (no dead keys) again: {}\n",
                field(&variant, "text")?,
                field(&composed, "text")?,
                field(&literal, "text")?,
            ),
        )?;

        // Enter in the test field goes on to the machine page.
        ctl.key("Return")?;
        let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
        ensure!(
            field(&machine, "selected")? == "generic.gnome"
                && field(&machine, "origin")? == "not-recognised",
            "the machine page after the welcome page: {machine:?}"
        );
        wait_page(vm, &ctl, artifacts, "kde-welcome-machine")?;
        ctl.save_screenshot(&artifacts.join("next-kde-welcome-machine.png"))?;
        Ok(())
    })
}

/// The welcome page to the machine page on a computer the scenario describes.
fn detect(
    purpose: &'static str,
    scenario: &str,
    board: bool,
    check: impl FnOnce(&std::collections::BTreeMap<String, String>) -> Result<()>,
) -> Result<()> {
    let smbios = if board {
        let ws = common::workspace()?;
        let table = installer::table(&ws.root, None)?;
        iso::smbios(&table, Some("ms-7c35"), true)?
    } else {
        vec![]
    };
    let booted = boot(purpose, scenario, smbios)?;
    run(booted, purpose, |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        welcome_up_of(KDE, vm, &ctl, &serial, artifacts, purpose)?;
        // Tab, Tab: the layout test field; Enter in it continues. (A simulated
        // session prints that field's text, so what a real boot must not print
        // is the real-helper trials' to check.)
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.key("Return")?;
        let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
        check(&machine)?;
        wait_page(vm, &ctl, artifacts, purpose)?;
        ctl.save_screenshot(&artifacts.join(format!("next-{purpose}.png")))?;
        Ok(())
    })
}

/// The pages from the welcome page to the administrator page filled in, by
/// keyboard: Enter in the layout test field, Enter on Continue (focused on the
/// machine and disk pages), the name field focused on the administrator page.
/// Enter in its last field would ask the helper to check the choices.
fn to_account_filled(
    vm: &mut ilex_devkit::qemu::Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    welcome_up_of(KDE, vm, ctl, serial, artifacts, name)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the machine page",
        1,
        EVENT_TIMEOUT,
        is_page("machine"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-machine"))?;
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the disk page",
        1,
        EVENT_TIMEOUT,
        is_page("disk"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-disk"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-disk.png")))?;
    ctl.key("Return")?;
    // The encryption page: on, with the layout the passphrase is typed in. The
    // passphrase field has the focus; a password field has two stops (its text
    // and the button that shows what was typed).
    let encryption = nth_marker(
        vm,
        serial,
        "the encryption page",
        1,
        EVENT_TIMEOUT,
        is_page("encryption"),
    )?;
    ensure!(
        field(&encryption, "encryption")? == "on" && field(&encryption, "keyboard")? == "us",
        "the encryption page: {encryption:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-encryption"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-encryption.png")))?;
    ctl.type_text(PASSPHRASE)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.type_text(PASSPHRASE)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the administrator page",
        1,
        EVENT_TIMEOUT,
        is_page("account"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-account"))?;
    // The name has the focus; Tab moves down the fields. A password field has
    // two stops: its text and the button that shows what was typed.
    ctl.type_text(ADMIN)?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-account-filled.png")))?;
    Ok(())
}

/// The pages from the welcome page to the review page by keyboard.
fn to_review(
    vm: &mut ilex_devkit::qemu::Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    to_account_filled(vm, ctl, serial, artifacts, name)?;
    // Enter in the last field continues: the helper (the simulation) plans.
    ctl.key("Return")?;
    let review = nth_marker(
        vm,
        serial,
        "the review page",
        1,
        Duration::from_secs(120),
        is_page("review"),
    )?;
    ensure!(
        field(&review, "check")? == "ok",
        "the plan was not checked: {review:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-review"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-review.png")))?;
    Ok(())
}

fn install() -> Result<()> {
    let booted = boot("iso-next-kde-install", "generic", vec![])?;
    run(booted, "kde-install", |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        to_review(vm, &ctl, &serial, artifacts, "kde-install")?;

        // The destructive button has the focus: Enter opens the dialog, whose
        // safe answer (Cancel) has the focus; Escape cancels and nothing began.
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the erase dialog",
            1,
            EVENT_TIMEOUT,
            is_event("confirm", "state", "shown"),
        )?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.save_screenshot(&artifacts.join("next-kde-install-erase-dialog.png"))?;
        ctl.key("Escape")?;
        nth_marker(
            vm,
            &serial,
            "the cancelled dialog",
            1,
            EVENT_TIMEOUT,
            is_event("confirm", "state", "cancelled"),
        )?;
        ensure!(
            count(&serial, is_event("install", "result", "started"))? == 0,
            "the installation began after the dialog was cancelled"
        );
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-install-erase-cancelled.png"))?;

        // Again: Tab moves from the safe answer to the destructive one, Enter
        // takes it (tried with one Tab, then two, as the GNOME trials do).
        confirm_erase(vm, &ctl, &serial, artifacts, "kde-install")?;
        nth_marker(
            vm,
            &serial,
            "the installing page",
            1,
            EVENT_TIMEOUT,
            is_page("installing"),
        )?;
        nth_marker(
            vm,
            &serial,
            "the done page",
            1,
            Duration::from_secs(180),
            is_page("done"),
        )?;
        wait_page(vm, &ctl, artifacts, "kde-install-done")?;
        ctl.save_screenshot(&artifacts.join("next-kde-install-done.png"))?;
        // Restart has the focus on the done page.
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the restart",
            1,
            EVENT_TIMEOUT,
            is_event("finish", "action", "restart"),
        )?;

        // The password is nowhere on the serial console...
        let text = console(&serial)?;
        ensure!(
            !text.contains(PASSWORD) && !text.contains(PASSPHRASE),
            "the password or the passphrase reached the serial console"
        );
        // ...and nowhere in the guest's journal, which holds the interface's own
        // marker lines (the patterns are written so that the command that
        // searches does not match itself).
        vm.wait_ssh(Duration::from_secs(180))?;
        let found = vm.ssh_ok(
            "journalctl -b --no-pager -o cat > /tmp/journal.txt; \
             echo markers=$(grep -c -e '^ILEX-INSTALLER page=review edition=kde' /tmp/journal.txt); \
             echo password=$(grep -c -e '[c]orrect horse battery' -e '[h]orse batt' /tmp/journal.txt); \
             echo passphrase=$(grep -c -e '[P]lant-the-' -e '[b]erries' /tmp/journal.txt); \
             rm -f /tmp/journal.txt",
        )?;
        ensure!(
            !found.contains("markers=0")
                && found.contains("password=0")
                && found.contains("passphrase=0"),
            "the journal holds the review page's marker and no password or passphrase: {found}"
        );
        Ok(())
    })
}

fn problem() -> Result<()> {
    let booted = boot("iso-next-kde-problem", "generic+fail-image+sticks", vec![])?;
    run(booted, "kde-problem", |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        to_review(vm, &ctl, &serial, artifacts, "kde-problem")?;
        confirm_erase(vm, &ctl, &serial, artifacts, "kde-problem")?;
        nth_marker(
            vm,
            &serial,
            "the failed outcome",
            1,
            Duration::from_secs(120),
            is_event("outcome", "result", "failed"),
        )?;
        let problem = nth_marker(
            vm,
            &serial,
            "the problem page",
            1,
            EVENT_TIMEOUT,
            is_page("problem"),
        )?;
        println!("iso::next kde problem page: {problem:?}");
        wait_page(vm, &ctl, artifacts, "kde-problem-page")?;
        ctl.save_screenshot(&artifacts.join("next-kde-problem.png"))?;

        // Try again has the focus; Tab reaches Save the log, Enter opens the
        // dialog, which lists the sticks.
        ctl.key("Tab")?;
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the log dialog",
            1,
            EVENT_TIMEOUT,
            is_event("log", "state", "choosing"),
        )?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.save_screenshot(&artifacts.join("next-kde-problem-save-log.png"))?;
        // Tab moves to a stick's Save here button (the first one that can take
        // the log), Enter writes it: tried with one Tab, then two, and so on.
        let mut saved = false;
        for attempt in 1..=4 {
            for _ in 0..attempt {
                ctl.key("Tab")?;
            }
            ctl.key("Return")?;
            std::thread::sleep(Duration::from_secs(3));
            if count(&serial, is_event("log", "state", "saved"))? > 0 {
                saved = true;
                break;
            }
            // The dialog is still open: start from the top of it again.
            ctl.key("Escape")?;
            std::thread::sleep(Duration::from_secs(1));
            ctl.key("Tab")?;
            ctl.key("Return")?;
            std::thread::sleep(Duration::from_secs(2));
        }
        ensure!(saved, "the log was not saved to a stick by keyboard");
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-problem-log-saved.png"))?;
        Ok(())
    })
}

/// Alt+Left is Kirigami's page row's own Back shortcut. While the plan is
/// checked (the simulation holds the check, `plan-slow`) the flow refuses to
/// go back, and the window must stay on the page the flow is on: the row is
/// put back and the flow does not move.
fn frozen_back() -> Result<()> {
    let booted = boot("iso-next-kde-frozen-back", "generic+plan-slow", vec![])?;
    run(booted, "kde-frozen-back", |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        to_account_filled(vm, &ctl, &serial, artifacts, "kde-frozen-back")?;
        // Enter asks the helper to check the choices: the administrator page
        // says "Checking..." and the flow is frozen until the answer, twelve
        // seconds on (the review page comes with it).
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        let page_markers = |w: &std::collections::BTreeMap<String, String>| {
            w.contains_key("page") && !w.contains_key("event")
        };
        let pages = count(&serial, page_markers)?;
        ctl.save_screenshot(&artifacts.join("next-kde-frozen-back-checking.png"))?;
        // Twice, a second apart: the row is lowered to the disk page, and put
        // back, each time.
        for _ in 0..2 {
            ctl.key("alt+Left")?;
            std::thread::sleep(Duration::from_secs(1));
        }
        ctl.save_screenshot(&artifacts.join("next-kde-frozen-back-after.png"))?;
        // The check ends where the flow was: on the review page, checked.
        let review = nth_marker(
            vm,
            &serial,
            "the review page",
            1,
            Duration::from_secs(60),
            is_page("review"),
        )?;
        ensure!(
            field(&review, "check")? == "ok",
            "the plan was not checked: {review:?}"
        );
        ensure!(
            count(&serial, page_markers)? == pages + 1 && count(&serial, is_page("account"))? == 1,
            "a page marker came besides the review page's while the plan was checked"
        );
        // The window said it put its page row back (the interface's own stderr
        // is in the journal): it had moved off the administrator page.
        vm.wait_ssh(Duration::from_secs(180))?;
        let found = vm.ssh_ok(
            "journalctl -b --no-pager -o cat > /tmp/journal.txt; \
             echo moved=$(grep -c -e 'qml: the page row showed .* while the flow is on account: put back' /tmp/journal.txt); \
             rm -f /tmp/journal.txt",
        )?;
        ensure!(
            !found.contains("moved=0"),
            "Alt+Left did not move the page row (or the window did not put it back): {found}"
        );
        wait_page(vm, &ctl, artifacts, "kde-frozen-back-review")?;
        ctl.save_screenshot(&artifacts.join("next-kde-frozen-back-review.png"))?;
        Ok(())
    })
}

/// The encryption page's refusals and the recovery key page, by keyboard, on
/// the simulated helper (`chip` makes this computer eligible for the security
/// chip, `recovery` makes the simulated helper hold the key, take it back
/// typed and offer the way out when it cannot be shown): the page the flow
/// shows is the one the trial looks at, whatever the helper behind it is.
fn encryption() -> Result<()> {
    let booted = boot(
        "iso-next-kde-encryption",
        "generic+chip+recovery+attach-key-shown",
        vec![],
    )?;
    run(booted, "kde-encryption", |vm, artifacts| {
        let serial = vm.info.dir.join("serial.log");
        let ctl = Vmctl::from_info(vm.info.clone());
        // An interface that starts on an installation whose key was taken by a
        // window that is gone: the recovery page with the way out. Nobody has
        // typed anything yet, so the field is empty and has the focus.
        let page = nth_marker(
            vm,
            &serial,
            "the recovery page",
            1,
            EVENT_TIMEOUT,
            is_page("recovery"),
        )?;
        ensure!(
            field(&page, "key")? == "gone" && field(&page, "skip")? == "true",
            "the recovery page of a window that was not given the key: {page:?}"
        );
        wait_page(vm, &ctl, artifacts, "kde-encryption-recovery")?;
        ctl.save_screenshot(&artifacts.join("next-kde-encryption-recovery.png"))?;
        // A key of the right shape and the wrong letter: refused (the field
        // empties; the screenshot records it).
        let refused = count(&serial, is_event("recovery", "state", "refused"))?;
        use ilex_devkit::installer_model::selfcheck::TEST_KEY;
        ctl.type_text(&format!("{}u", &TEST_KEY[..TEST_KEY.len() - 1]))?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the helper's refusal",
            refused + 1,
            EVENT_TIMEOUT,
            is_event("recovery", "state", "refused"),
        )?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.save_screenshot(&artifacts.join("next-kde-encryption-recovery-refused.png"))?;
        // The way out: Tab reaches it, Enter takes it.
        ctl.key("Tab")?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join("next-kde-encryption-recovery-skip.png"))?;
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "going on without the key",
            1,
            EVENT_TIMEOUT,
            is_event("recovery", "state", "declined"),
        )?;
        let text = console(&serial)?;
        ensure!(
            !text.contains(PASSPHRASE),
            "the passphrase reached the serial console"
        );
        Ok(())
    })
}

/// A secure VM (KVM with SMM, Secure Boot, a persistent TPM) installs the
/// edition's composition for real, the person at the Kirigami interface, and
/// the rest is what `iso::next_security` asserts.
fn security(variant: Variant) -> Result<()> {
    let name = match variant {
        Variant::Confirmed => "kde-security",
        Variant::Declined => "kde-security-declined",
        Variant::Off => anyhow::bail!("no such trial"),
    };
    security::scenario_of(
        &Edit {
            edition: KDE,
            variant,
            name,
            target: "ghcr.io/reidond/ilex:generic.kde",
            // The installer's own default (the interface asks for no name).
            host: "ilex",
        },
        ui_install,
    )
}

/// The installation by keys: every page, the erase dialog cancelled once and
/// then confirmed, the recovery key page (the trial takes the key from the
/// helper while the interface is stopped, as the GNOME trials do, so the page
/// is the one a restarted window shows), the end, Power off. Returns the key.
fn ui_install(
    machine: &mut Machine,
    variant: Variant,
    artifacts: &Path,
    name: &str,
    secrets: &mut Vec<String>,
) -> Result<String> {
    let give_up = variant == Variant::Declined;
    let started = std::time::Instant::now();
    let vm = machine.vm()?;
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    vm.wait_ssh(Duration::from_secs(600))
        .context("the live system did not come up")?;
    // The computer, as `ilex-enroll` sees it: eligible with Secure Boot on and
    // a TPM whose PCR 7 is what its event log replays to.
    let probe: serde_json::Value = serde_json::from_str(&vm.ssh_ok("ilex-enroll probe --json")?)
        .context("ilex-enroll's probe")?;
    ensure!(
        probe["verdict"] == "eligible"
            && probe["secure_boot"] == true
            && probe["tpm2"] == true
            && probe["pcr7"] == probe["replay"]["sha256"],
        "the secure VM is not an eligible computer: {probe}"
    );
    let outcome = (|| -> Result<(String, String)> {
        welcome_up_of(KDE, vm, &ctl, &serial, artifacts, name)?;
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the machine page",
            1,
            EVENT_TIMEOUT,
            is_page("machine"),
        )?;
        wait_page(vm, &ctl, artifacts, &format!("{name}-machine"))?;
        ctl.key("Return")?;
        let disk = nth_marker(
            vm,
            &serial,
            "the disk page",
            1,
            EVENT_TIMEOUT,
            is_page("disk"),
        )?;
        ensure!(
            field(&disk, "offered")? == "1" && field(&disk, "disk")? == "virtio-ilex-disk-0",
            "the disk page of a VM with one disk: {disk:?}"
        );
        wait_page(vm, &ctl, artifacts, &format!("{name}-disk"))?;
        ctl.key("Return")?;
        // The encryption page: on, with the security chip's check box on and
        // enabled (this computer is eligible), and the passphrase typed twice.
        let encryption = nth_marker(
            vm,
            &serial,
            "the encryption page",
            1,
            EVENT_TIMEOUT,
            is_page("encryption"),
        )?;
        ensure!(
            field(&encryption, "encryption")? == "on"
                && field(&encryption, "keyboard")? == "us"
                && field(&encryption, "chip")? == "on",
            "the encryption page of an eligible computer: {encryption:?}"
        );
        wait_page(vm, &ctl, artifacts, &format!("{name}-encryption"))?;
        ctl.save_screenshot(&artifacts.join(format!("next-{name}-encryption.png")))?;
        ctl.type_text(US.keys)?;
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.type_text(US.keys)?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the administrator page",
            1,
            EVENT_TIMEOUT,
            is_page("account"),
        )?;
        wait_page(vm, &ctl, artifacts, &format!("{name}-account"))?;
        ctl.type_text(apply::ADMIN)?;
        ctl.key("Tab")?;
        ctl.type_text(apply::PASSWORD)?;
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.type_text(apply::PASSWORD)?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
        let review = nth_marker(
            vm,
            &serial,
            "the review page",
            1,
            Duration::from_secs(240),
            is_page("review"),
        )?;
        ensure!(
            field(&review, "check")? == "ok",
            "the plan was not checked: {review:?}"
        );
        wait_page(vm, &ctl, artifacts, &format!("{name}-review"))?;
        ctl.save_screenshot(&artifacts.join(format!("next-{name}-review.png")))?;
        // Escape closes the dialog: nothing is asked of the helper.
        ctl.key("Return")?;
        nth_marker(
            vm,
            &serial,
            "the erase dialog",
            1,
            EVENT_TIMEOUT,
            is_event("confirm", "state", "shown"),
        )?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.key("Escape")?;
        nth_marker(
            vm,
            &serial,
            "the cancelled dialog",
            1,
            EVENT_TIMEOUT,
            is_event("confirm", "state", "cancelled"),
        )?;
        ensure!(
            helper_state(vm)?.applied.is_none(),
            "the helper began after the dialog was cancelled"
        );
        confirm_erase(vm, &ctl, &serial, artifacts, name)?;
        let key = wait_done(vm, &ctl, &serial, artifacts, name, give_up)?;
        let state = helper_state(vm)?;
        let summary = state
            .summary
            .context("the helper has no summary of what it installed")?;
        ensure!(
            matches!(
                state.outcome,
                Some(ilex_devkit::installer_model::state::Outcome::Success)
            ) && summary.digest.starts_with("sha256:")
                && summary.composition == "generic.kde"
                && summary.disk == "virtio-ilex-disk-0",
            "the helper's record: {summary:?} {:?}",
            state.outcome
        );
        Ok((key, summary.digest))
    })();
    let (key, digest) = match outcome {
        Ok(done) => done,
        Err(error) => {
            diagnostics(vm, artifacts, name);
            return Err(error);
        }
    };
    secrets.push(key.clone());
    // What the boots check the installed digest against.
    std::fs::write(
        artifacts.join(format!("next-{name}-install.txt")),
        format!("PLAN {{\"digest\":\"{digest}\"}}\n"),
    )?;
    no_password(vm, &serial, Some(&key))?;
    // Enforcing and not one denial through the whole installation, asked before
    // the machine goes down.
    selinux::check(vm, artifacts, &format!("next-{name}"))?;
    // The done page's Restart has the focus; Tab reaches Power off.
    ctl.key("Tab")?;
    ctl.key("Return")?;
    machine.power_off().context("powering the installer off")?;
    println!(
        "iso::next {name}: installed and powered off in {:?}",
        started.elapsed()
    );
    Ok(key)
}
