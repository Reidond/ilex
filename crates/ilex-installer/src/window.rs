//! The placeholder page: GTK 4 + libadwaita through relm4.

use std::fs;
use std::time::Duration;

use anyhow::Result;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::prelude::*;

use crate::identity::Page;
use crate::serial;

const APP_ID: &str = "org.ilex.Installer";
/// Memory is sampled this long after the page is shown, once the session has
/// settled, for the boot report.
const STATS_DELAY: Duration = Duration::from_secs(15);

pub fn run(page: Page, fullscreen: bool) -> Result<()> {
    // The command line is ours, not GTK's.
    RelmApp::new(APP_ID)
        .with_args(Vec::new())
        .run::<Installer>(Init { page, fullscreen });
    Ok(())
}

struct Init {
    page: Page,
    fullscreen: bool,
}

/// The installer's only component for now. Later pages add their own.
struct Installer;

impl SimpleComponent for Installer {
    type Init = Init;
    type Input = ();
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("ilex installer")
            .default_width(1024)
            .default_height(640)
            .build()
    }

    fn init(init: Init, root: Self::Root, _sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let Init { page, fullscreen } = init;
        let machine = &page.machine;

        let group = adw::PreferencesGroup::builder()
            .title("This computer")
            .build();
        group.add(&property("Computer", &machine.computer()));
        group.add(&property("Board", &machine.board()));
        let status = adw::StatusPage::builder()
            .icon_name("computer-symbolic")
            .title(&page.edition.display_name)
            .description("This is a placeholder: nothing is installed yet.")
            .child(
                &adw::Clamp::builder()
                    .maximum_size(520)
                    .child(&group)
                    .build(),
            )
            .build();
        let content = adw::ToolbarView::new();
        content.set_content(Some(&status));
        root.set_content(Some(&content));
        root.set_fullscreened(fullscreen);

        let edition = page.edition.edition;
        root.connect_map(move |window| announce(window, &edition));
        ComponentParts {
            model: Installer,
            widgets: (),
        }
    }
}

/// A read-only "title above value" row (libadwaita's `property` style).
fn property(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(true)
        .build();
    row.add_css_class("property");
    row
}

/// The tests' signal that the page is on screen, and which renderer GTK chose.
fn announce(window: &adw::ApplicationWindow, edition: &str) {
    let renderer = window
        .native()
        .and_then(|native| native.renderer())
        .map_or_else(|| "none".to_owned(), |r| r.type_().name().to_owned());
    serial::marker(&format!(
        "ILEX-INSTALLER page=placeholder edition={edition} renderer={renderer} scale={} uptime={}",
        window.scale_factor(),
        uptime()
    ));
    glib::timeout_add_local_once(STATS_DELAY, || {
        let meminfo = |key: &str| {
            fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                        .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
                })
                .unwrap_or_else(|| "unknown".into())
        };
        serial::marker(&format!(
            "ILEX-INSTALLER-STATS mem_total_kib={} mem_available_kib={} shmem_kib={} uptime={}",
            meminfo("MemTotal"),
            meminfo("MemAvailable"),
            meminfo("Shmem"),
            uptime()
        ));
    });
}

/// Seconds since boot, from the kernel.
fn uptime() -> String {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}
