//! The pages from the disk to the end: disk, review, installing, done and
//! problem, and the two dialogs (the erase confirmation and the log). Each is
//! drawn from what `ilex-installer-flow` says (`Flow::disk_view`,
//! `Flow::review`, `Flow::installing_view`, `Flow::done_view`,
//! `Flow::problem_view`, `Flow::log_state`); nothing here decides anything.

use super::app::{
    App, Msg, banner, continue_button, form, group, message_label, page, property_row, secret_of,
    sentence, show_message, sync_banner,
};
use ilex_installer_flow::disks as flow_disks;
use ilex_installer_flow::flow::{
    Control, Flow, LogSave, ProbeState, ProblemIcon, ProblemKind, ReviewBody,
};
use ilex_installer_flow::messages;
use ilex_installer_flow::progress::Status;
use ilex_installer_flow::review::Confirmation;
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

/// A group of read-only rows (title and value), rebuilt when its content
/// changes.
pub struct Rows {
    pub group: adw::PreferencesGroup,
    rows: Vec<adw::ActionRow>,
    shown: Vec<(String, String)>,
}

impl Rows {
    pub fn new(title: &str, description: &str) -> Self {
        Self {
            group: group(title, description),
            rows: Vec::new(),
            shown: Vec::new(),
        }
    }

    pub fn set(&mut self, items: &[(String, String)]) {
        if self.shown == items {
            return;
        }
        for row in self.rows.drain(..) {
            self.group.remove(&row);
        }
        for (title, value) in items {
            let row = property_row(title);
            row.set_subtitle(value);
            row.set_subtitle_lines(0);
            self.group.add(&row);
            self.rows.push(row);
        }
        self.shown = items.to_vec();
        self.group.set_visible(!items.is_empty());
    }
}

fn pair<T: AsRef<str>>(title: &str, value: T) -> (String, String) {
    (title.to_owned(), value.as_ref().to_owned())
}

fn status_page(icon: &str) -> adw::StatusPage {
    adw::StatusPage::builder().icon_name(icon).build()
}

/// The layout of a page an installation ends on (what `AdwStatusPage` is, with
/// the icon coloured on its own: the style class on the whole page would
/// recolour its buttons too).
struct Closing {
    scroll: gtk::ScrolledWindow,
    icon: gtk::Image,
    title: gtk::Label,
    detail: gtk::Label,
    body: gtk::Box,
}

impl Closing {
    fn new(icon: &str, class: &str, width: i32) -> Self {
        let icon = gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(96)
            .css_classes([class])
            .build();
        let title = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["title-1"])
            .build();
        let detail = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["body"])
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 14);
        column.append(&icon);
        column.append(&title);
        column.append(&detail);
        column.append(&body);
        let clamp = adw::Clamp::builder()
            .maximum_size(width)
            .tightening_threshold(width - 160)
            .margin_top(48)
            .margin_bottom(24)
            .margin_start(18)
            .margin_end(18)
            .valign(gtk::Align::Center)
            .child(&column)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        Self {
            scroll,
            icon,
            title,
            detail,
            body,
        }
    }

    fn set(&self, title: &str, detail: &str) {
        self.title.set_label(title);
        self.detail.set_label(detail);
    }

    fn set_icon(&self, icon: &str) {
        self.icon.set_icon_name(Some(icon));
    }
}

fn pill(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("pill");
    button.set_halign(gtk::Align::Center);
    button
}

fn spinning(title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .title(title)
        .description(description)
        .child(&adw::Spinner::new())
        .build()
}

/// The last lines of what the tools printed, as a read-only monospace view.
fn log_view() -> (gtk::TextView, gtk::ScrolledWindow) {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(10)
        .right_margin(10)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    view.add_css_class("card");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(140)
        .max_content_height(220)
        .propagate_natural_height(true)
        .child(&view)
        .build();
    (view, scroll)
}

fn set_log(view: &gtk::TextView, lines: &[String]) {
    let text = lines.join("\n");
    let buffer = view.buffer();
    let (start, end) = buffer.bounds();
    if buffer.text(&start, &end, false) != text {
        buffer.set_text(&text);
    }
}

// --- disk ---------------------------------------------------------------------------

pub struct DiskPage {
    pub page: adw::NavigationPage,
    banner: adw::Banner,
    /// Why the page is showing again: the disk that was chosen is gone.
    notice: adw::Banner,
    stack: gtk::Stack,
    none: adw::StatusPage,
    group: adw::PreferencesGroup,
    rows: Vec<(String, gtk::CheckButton, adw::ActionRow)>,
    away: adw::PreferencesGroup,
    away_rows: Vec<adw::ActionRow>,
    shown: Vec<(String, bool)>,
    pub button: gtk::Button,
}

impl DiskPage {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let banner = banner(None);
        let notice = adw::Banner::new("");
        let button = continue_button("Continue");
        let group = group("Disk", "");
        let away = group_titled("Not available", "ilex cannot be installed to these disks.");
        let modes = adw::PreferencesGroup::builder().title("How").build();
        let mut first: Option<gtk::CheckButton> = None;
        for mode in flow_disks::modes() {
            let check = gtk::CheckButton::new();
            if let Some(first) = &first {
                check.set_group(Some(first));
            } else {
                first = Some(check.clone());
            }
            check.set_active(mode.available);
            let row = adw::ActionRow::builder()
                .title(mode.title)
                .subtitle(mode.subtitle)
                .subtitle_lines(0)
                .activatable_widget(&check)
                .build();
            row.add_prefix(&check);
            // This version erases the whole disk: the row is shown as the way
            // that is used, and the other as one that comes later.
            row.set_sensitive(mode.available);
            modes.add(&row);
        }
        let content = form(
            "Where to install",
            "Choose the disk ilex is installed on. Nothing is changed on it until you confirm on the last page.",
            &[group.upcast_ref(), away.upcast_ref(), modes.upcast_ref()],
        );
        let none = status_page("drive-harddisk-symbolic");
        let again = pill("Try again");
        none.set_child(Some(&again));
        let input = sender.input_sender().clone();
        again.connect_clicked(move |_| input.emit(Msg::Probe));
        let loading = spinning(
            "Looking at the disks",
            "The installer is reading which disks this computer has.",
        );
        let stack = gtk::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&none, Some("none"));
        stack.add_named(&content, Some("choices"));
        let page = page("Disk", "disk", Some(&banner), &stack, Some(&button));
        // The notice sits under the helper's banner, in the page's top bar.
        if let Some(toolbar) = page.child().and_downcast::<adw::ToolbarView>() {
            toolbar.add_top_bar(&notice);
        }
        let input = sender.input_sender().clone();
        banner.connect_button_clicked(move |_| input.emit(Msg::Probe));
        let input = sender.input_sender().clone();
        button.connect_clicked(move |_| input.emit(Msg::Continue));
        Self {
            page,
            banner,
            notice,
            stack,
            none,
            group,
            rows: Vec::new(),
            away,
            away_rows: Vec::new(),
            shown: Vec::new(),
            button,
        }
    }

    pub fn sync(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        sync_banner(&self.banner, flow);
        match flow.disk_notice() {
            Some(message) => {
                self.notice.set_title(&sentence(&message));
                self.notice.set_revealed(true);
            }
            None => self.notice.set_revealed(false),
        }
        self.button.set_label(flow.continue_label());
        self.button
            .set_sensitive(flow.page() != ilex_installer_flow::PageId::Disk || flow.can_continue());
        match (flow.probe_state(), flow.disk_view()) {
            (ProbeState::Pending, _) => self.stack.set_visible_child_name("loading"),
            (ProbeState::Failed(message), _) => {
                self.none.set_icon_name(Some("dialog-error-symbolic"));
                self.none.set_title(&message.title);
                self.none.set_description(Some(&message.detail));
                self.stack.set_visible_child_name("none");
            }
            (ProbeState::Ready(_), Some(view)) => {
                if let Some(problem) = flow.disk_problem() {
                    self.none.set_icon_name(Some("drive-harddisk-symbolic"));
                    self.none.set_title(&problem.title);
                    self.none.set_description(Some(&problem.detail));
                    self.stack.set_visible_child_name("none");
                    return;
                }
                self.stack.set_visible_child_name("choices");
                let key: Vec<(String, bool)> = view
                    .choices
                    .iter()
                    .map(|c| (c.id.clone(), c.is_offered()))
                    .collect();
                if key != self.shown {
                    for (_, _, row) in self.rows.drain(..) {
                        self.group.remove(&row);
                    }
                    for row in self.away_rows.drain(..) {
                        self.away.remove(&row);
                    }
                    let mut first: Option<gtk::CheckButton> = None;
                    for choice in &view.choices {
                        let row = adw::ActionRow::builder()
                            .title(&choice.title)
                            .subtitle(choice.lines().join("\n"))
                            .subtitle_lines(0)
                            .build();
                        if choice.is_offered() {
                            let check = gtk::CheckButton::new();
                            if let Some(first) = &first {
                                check.set_group(Some(first));
                            } else {
                                first = Some(check.clone());
                            }
                            row.add_prefix(&check);
                            row.set_activatable_widget(Some(&check));
                            let id = choice.id.clone();
                            let input = sender.input_sender().clone();
                            check.connect_toggled(move |check| {
                                if check.is_active() {
                                    input.emit(Msg::Disk(id.clone()));
                                }
                            });
                            self.group.add(&row);
                            self.rows.push((choice.id.clone(), check, row));
                        } else {
                            row.set_sensitive(false);
                            self.away.add(&row);
                            self.away_rows.push(row);
                        }
                    }
                    self.away.set_visible(!self.away_rows.is_empty());
                    self.shown = key;
                }
                let selected = flow.disk().unwrap_or_default();
                for (id, check, _) in &self.rows {
                    if check.is_active() != (id == selected) {
                        check.set_active(id == selected);
                    }
                }
            }
            (ProbeState::Ready(_), None) => {}
        }
    }
}

fn group_titled(title: &str, description: &str) -> adw::PreferencesGroup {
    group(title, description)
}

// --- review -------------------------------------------------------------------------

pub struct ReviewPage {
    pub page: adw::NavigationPage,
    stack: gtk::Stack,
    running: adw::StatusPage,
    problem: adw::StatusPage,
    pub retry: gtk::Button,
    choices: Rows,
    disk: Rows,
    partitions: Rows,
    image: Rows,
    notes: Rows,
    erase: gtk::Label,
    pub button: gtk::Button,
    /// The checked review was showing at the last sync: the button takes the
    /// focus when it appears (a page that was already shown, after Try again,
    /// has no other moment to).
    checked: bool,
}

impl ReviewPage {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let running = spinning(messages::CHECKING_TITLE, messages::CHECKING_DETAIL);
        let problem = adw::StatusPage::new();
        let retry = pill("Try again");
        problem.set_child(Some(&retry));
        let mut choices = Rows::new("Your choices", "");
        let mut disk = Rows::new("The disk", "");
        let mut partitions = Rows::new("What is created", "These partitions are made on the disk.");
        let mut image = Rows::new("The image", "");
        let mut notes = Rows::new("Worth knowing", "");
        for rows in [
            &mut choices,
            &mut disk,
            &mut partitions,
            &mut image,
            &mut notes,
        ] {
            rows.group.set_visible(false);
        }
        let erase = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading", "error"])
            .build();
        let end = gtk::Label::builder()
            .label("Nothing has been written to any disk yet. The button below is the only way on.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let checked = form(
            "Review",
            "Check what will be installed. Go back to change anything.",
            &[
                choices.group.upcast_ref(),
                disk.group.upcast_ref(),
                partitions.group.upcast_ref(),
                image.group.upcast_ref(),
                notes.group.upcast_ref(),
                erase.upcast_ref(),
                end.upcast_ref(),
            ],
        );
        let stack = gtk::Stack::new();
        stack.add_named(&running, Some("running"));
        stack.add_named(&problem, Some("problem"));
        stack.add_named(&checked, Some("checked"));
        let button = gtk::Button::with_label("Erase and install");
        button.add_css_class("destructive-action");
        button.set_margin_top(6);
        button.set_margin_bottom(6);
        button.set_visible(false);
        let page = page("Review", "review", None, &stack, Some(&button));
        let input = sender.input_sender().clone();
        button.connect_clicked(move |_| input.emit(Msg::Install));
        let input = sender.input_sender().clone();
        retry.connect_clicked(move |_| input.emit(Msg::Retry));
        Self {
            page,
            stack,
            running,
            problem,
            retry,
            choices,
            disk,
            partitions,
            image,
            notes,
            erase,
            button,
            checked: false,
        }
    }

    pub fn sync(&mut self, flow: &Flow) {
        let was_checked = std::mem::take(&mut self.checked);
        let view = flow.review_page();
        apply(&self.button, view.button);
        match &view.body {
            ReviewBody::Waiting(message) => {
                self.running.set_title(&message.title);
                self.running.set_description(Some(&message.detail));
                self.stack.set_visible_child_name("running");
            }
            ReviewBody::Checked(review) => {
                let items = |rows: &[ilex_installer_flow::review::Row]| -> Vec<(String, String)> {
                    rows.iter().map(|r| pair(r.title, &r.value)).collect()
                };
                self.choices.set(&items(&review.choices));
                self.disk.set(&items(&review.disk));
                let partitions: Vec<(String, String)> = review
                    .partitions
                    .iter()
                    .map(|p| pair(&p.title, &p.detail))
                    .collect();
                self.partitions.set(&partitions);
                self.image.set(&items(&review.image));
                self.notes.set(&items(&review.notes));
                self.erase.set_label(&review.erase);
                self.button.set_label(&review.button);
                self.stack.set_visible_child_name("checked");
                self.checked = true;
                if !was_checked && flow.page() == ilex_installer_flow::PageId::Review {
                    self.button.grab_focus();
                }
            }
            ReviewBody::Problem {
                message,
                icon,
                retry,
            } => {
                self.problem.set_icon_name(Some(match icon {
                    ProblemIcon::Disk => "drive-harddisk-symbolic",
                    ProblemIcon::Error => "dialog-error-symbolic",
                }));
                self.problem.set_title(&message.title);
                self.problem.set_description(Some(&message.detail));
                apply(&self.retry, *retry);
                self.stack.set_visible_child_name("problem");
            }
        }
    }
}

/// Show and enable a button as the flow says.
fn apply(button: &gtk::Button, control: Control) {
    button.set_visible(control.visible);
    button.set_sensitive(control.enabled);
}

/// The erase confirmation: the dialog's heading and body say which disk, and
/// the focus starts on the safe answer (Cancel, the first button). It is an
/// `AdwDialog` with two buttons of its own, so that the keyboard order is ours:
/// Tab moves from Cancel to the destructive button, Escape closes the dialog
/// (which is a cancellation: the flow ignores the close that follows an answer).
pub fn confirmation(dialog: &Confirmation, sender: &ComponentSender<App>) -> adw::Dialog {
    let title = gtk::Label::builder()
        .label(&dialog.title)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["title-2"])
        .build();
    let body = gtk::Label::builder()
        .label(&dialog.body)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let erase = gtk::Button::with_label(&dialog.confirm);
    erase.add_css_class("destructive-action");
    erase.add_css_class("pill");
    let cancel = gtk::Button::with_label(dialog.cancel);
    cancel.add_css_class("pill");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
    column.set_margin_top(24);
    column.set_margin_bottom(24);
    column.set_margin_start(24);
    column.set_margin_end(24);
    // Cancel first: the focus starts on it and Tab reaches the destructive
    // answer, never the other way round.
    for widget in [
        title.upcast_ref::<gtk::Widget>(),
        body.upcast_ref(),
        cancel.upcast_ref(),
        erase.upcast_ref(),
    ] {
        column.append(widget);
    }
    title.set_max_width_chars(34);
    body.set_max_width_chars(44);
    let alert = adw::Dialog::builder()
        .content_width(420)
        .child(&column)
        .build();
    alert.set_default_widget(Some(&cancel));
    alert.set_focus(Some(&cancel));
    let input = sender.input_sender().clone();
    erase.connect_clicked({
        let alert = alert.clone();
        move |_| {
            input.emit(Msg::Confirmed(true));
            alert.force_close();
        }
    });
    let input = sender.input_sender().clone();
    cancel.connect_clicked({
        let alert = alert.clone();
        move |_| {
            input.emit(Msg::Confirmed(false));
            alert.force_close();
        }
    });
    // Escape, or any other way the dialog goes away: not confirmed. After an
    // answer the flow has moved on and ignores this.
    let input = sender.input_sender().clone();
    alert.connect_closed(move |_| input.emit(Msg::Confirmed(false)));
    alert
}

// --- installing ---------------------------------------------------------------------

struct StageRow {
    stage: ilex_installer_model::state::Stage,
    row: adw::ActionRow,
    icon: gtk::Stack,
    bar: gtk::ProgressBar,
}

pub struct InstallingPage {
    pub page: adw::NavigationPage,
    banner: adw::Banner,
    heading: gtk::Label,
    overall: gtk::ProgressBar,
    message: gtk::Label,
    group: adw::PreferencesGroup,
    rows: Vec<StageRow>,
    log: gtk::TextView,
}

impl InstallingPage {
    pub fn build() -> Self {
        let banner = banner(None);
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-1"])
            .build();
        let body = gtk::Label::builder()
            .label("The image is downloaded from the registry and written to the disk. This takes a while; leave the computer on.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body"])
            .build();
        let overall = gtk::ProgressBar::new();
        let message = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let group = group("", "");
        let (log, scroll) = log_view();
        let expander = gtk::Expander::builder()
            .label("Show the log")
            .child(&scroll)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        for widget in [
            heading.upcast_ref::<gtk::Widget>(),
            body.upcast_ref(),
            overall.upcast_ref(),
            message.upcast_ref(),
            group.upcast_ref(),
            expander.upcast_ref(),
        ] {
            content.append(widget);
        }
        let clamp = adw::Clamp::builder()
            .maximum_size(620)
            .tightening_threshold(460)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(18)
            .margin_end(18)
            .child(&content)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        let page = page("Installing", "installing", Some(&banner), &scrolled, None);
        Self {
            page,
            banner,
            heading,
            overall,
            message,
            group,
            rows: Vec::new(),
            log,
        }
    }

    pub fn sync(&mut self, flow: &Flow) {
        let Some(view) = flow.installing_view() else {
            return;
        };
        self.heading.set_label(&view.heading);
        self.overall.set_fraction(view.overall);
        self.message.set_label(&view.message);
        match &view.waiting {
            Some(waiting) => {
                self.banner.set_title(&sentence(waiting));
                self.banner.set_revealed(true);
            }
            None => self.banner.set_revealed(false),
        }
        let stages: Vec<_> = view.rows.iter().map(|r| r.stage).collect();
        if self.rows.iter().map(|r| r.stage).collect::<Vec<_>>() != stages {
            for row in self.rows.drain(..) {
                self.group.remove(&row.row);
            }
            for row in &view.rows {
                let icon = gtk::Stack::new();
                let pending = gtk::Image::from_icon_name("media-record-symbolic");
                pending.add_css_class("dim-label");
                icon.add_named(&pending, Some("pending"));
                icon.add_named(&adw::Spinner::new(), Some("running"));
                let done = gtk::Image::from_icon_name("object-select-symbolic");
                done.add_css_class("success");
                icon.add_named(&done, Some("done"));
                let failed = gtk::Image::from_icon_name("dialog-error-symbolic");
                failed.add_css_class("error");
                icon.add_named(&failed, Some("failed"));
                let bar = gtk::ProgressBar::builder()
                    .valign(gtk::Align::Center)
                    .width_request(120)
                    .visible(false)
                    .build();
                let widget = adw::ActionRow::builder().title(row.label).build();
                widget.add_prefix(&icon);
                widget.add_suffix(&bar);
                self.group.add(&widget);
                self.rows.push(StageRow {
                    stage: row.stage,
                    row: widget,
                    icon,
                    bar,
                });
            }
        }
        for (row, shown) in view.rows.iter().zip(&self.rows) {
            let (name, bar) = match row.status {
                Status::Pending => ("pending", false),
                Status::Running => ("running", true),
                Status::Done => ("done", false),
                Status::Failed => ("failed", false),
            };
            shown.icon.set_visible_child_name(name);
            shown.bar.set_visible(bar);
            shown.bar.set_fraction(row.fraction);
        }
        set_log(&self.log, &view.log);
    }
}

// --- recovery key ----------------------------------------------------------------------

/// The recovery key page: the key in groups (while this interface holds it),
/// then the two groups asked for. What it shows is `Flow::recovery_view`.
pub struct RecoveryPage {
    pub page: adw::NavigationPage,
    heading: gtk::Label,
    intro: gtk::Label,
    /// The key, its warning and where to keep it: shown while this interface
    /// holds the key.
    key_box: gtk::Box,
    warning: gtk::Label,
    cells: Vec<gtk::Label>,
    keep: gtk::Label,
    /// Why there is no key to show (being fetched, shown before a restart).
    no_key: gtk::Label,
    ask: gtk::Label,
    rows: [adw::EntryRow; 2],
    messages: [gtk::Label; 2],
    problem: gtk::Label,
    progress: gtk::ProgressBar,
    line: gtk::Label,
    pub button: gtk::Button,
    /// The flow's wipe count the fields were last cleared at.
    wiped: u64,
}

impl RecoveryPage {
    pub fn build(sender: &ComponentSender<App>, wiped: u64) -> Self {
        let heading = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-1"])
            .build();
        let intro = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body"])
            .build();
        // The key, eight groups in two rows of four, numbered as the fields
        // below name them. Not selectable: nothing copies it to a clipboard.
        let warning = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading", "warning"])
            .build();
        let grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(18)
            .halign(gtk::Align::Center)
            .build();
        let mut cells = Vec::new();
        for index in 0..ilex_installer_model::recovery::GROUPS {
            let cell = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let number = gtk::Label::builder()
                .label(format!("{}", index + 1))
                .css_classes(["caption", "dim-label"])
                .build();
            let group = gtk::Label::builder()
                .css_classes(["title-1", "monospace"])
                .selectable(false)
                .build();
            group.set_widget_name(&format!("recovery-key-group-{}", index + 1));
            cell.append(&number);
            cell.append(&group);
            grid.attach(&cell, (index % 4) as i32, (index / 4) as i32, 1, 1);
            cells.push(group);
        }
        let keep = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let key_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        key_box.add_css_class("card");
        key_box.set_margin_top(6);
        for widget in [
            warning.upcast_ref::<gtk::Widget>(),
            grid.upcast_ref(),
            keep.upcast_ref(),
        ] {
            widget.set_margin_start(18);
            widget.set_margin_end(18);
            key_box.append(widget);
        }
        warning.set_margin_top(18);
        keep.set_margin_bottom(18);
        let no_key = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body", "warning"])
            .visible(false)
            .build();
        let ask = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body"])
            .build();
        let make_row = |slot: usize| {
            let row = adw::EntryRow::builder()
                .name(format!("recovery-group-{}", slot + 1))
                .build();
            row.set_enable_undo(false);
            row.set_input_purpose(gtk::InputPurpose::FreeForm);
            row
        };
        let rows = [make_row(0), make_row(1)];
        let fields = group("", "");
        fields.add(&rows[0]);
        fields.add(&rows[1]);
        let messages = [message_label(), message_label()];
        let problem = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "error"])
            .visible(false)
            .build();
        let progress = gtk::ProgressBar::new();
        let line = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let button = continue_button("Confirm");
        let content = form_content(
            &heading,
            &intro,
            &[
                key_box.upcast_ref::<gtk::Widget>(),
                no_key.upcast_ref(),
                ask.upcast_ref(),
                fields.upcast_ref(),
                messages[0].upcast_ref(),
                messages[1].upcast_ref(),
                problem.upcast_ref(),
                progress.upcast_ref(),
                line.upcast_ref(),
            ],
        );
        let page = page("Recovery key", "recovery", None, &content, Some(&button));
        let input = sender.input_sender().clone();
        for (slot, row) in rows.iter().enumerate() {
            row.connect_changed({
                let input = input.clone();
                move |row| input.emit(Msg::RecoveryGroup(slot, secret_of(row)))
            });
            row.connect_entry_activated({
                let input = input.clone();
                move |_| input.emit(Msg::ConfirmRecovery)
            });
        }
        button.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::ConfirmRecovery)
        });
        // The first field has the focus when the page is shown.
        page.connect_shown({
            let first = rows[0].clone();
            move |_| {
                first.grab_focus();
            }
        });
        Self {
            page,
            heading,
            intro,
            key_box,
            warning,
            cells,
            keep,
            no_key,
            ask,
            rows,
            messages,
            problem,
            progress,
            line,
            button,
            wiped,
        }
    }

    pub fn sync(&mut self, flow: &Flow) {
        // The flow wiped the groups it holds (confirmed, refused, or the
        // installation ended): the fields are cleared too.
        if self.wiped != flow.secrets_wiped() {
            self.wiped = flow.secrets_wiped();
            for row in &self.rows {
                row.set_text("");
            }
        }
        let Some(view) = flow.recovery_view() else {
            // Not the page: nothing of the key stays in a widget.
            for cell in &self.cells {
                cell.set_label("");
            }
            return;
        };
        self.heading.set_label(view.heading);
        self.intro.set_label(view.introduction);
        match &view.groups {
            Some(groups) => {
                for (cell, group) in self.cells.iter().zip(groups) {
                    cell.set_label(group);
                }
                self.warning.set_label(view.warning.unwrap_or_default());
                self.keep.set_label(view.keep.unwrap_or_default());
                self.key_box.set_visible(true);
            }
            None => {
                for cell in &self.cells {
                    cell.set_label("");
                }
                self.key_box.set_visible(false);
            }
        }
        match &view.no_key {
            Some(message) => {
                self.no_key.set_label(&sentence(message));
                self.no_key.set_visible(true);
            }
            None => self.no_key.set_visible(false),
        }
        self.ask.set_label(view.ask);
        // The fields are asked in the order of the numbers.
        for (slot, (row, asked)) in self.rows.iter().zip(&view.asked).enumerate() {
            // The fields stay as they are while the helper compares (an
            // insensitive field would give up the focus, and a refusal would
            // leave a person with none): the flow ignores what is typed meanwhile.
            row.set_title(&asked.label);
            show_message(
                &self.messages[slot],
                asked.check.message(),
                &[row.upcast_ref::<gtk::Widget>()],
            );
        }
        match &view.problem {
            Some(problem) => {
                self.problem.set_label(&sentence(problem));
                self.problem.set_visible(true);
            }
            None => self.problem.set_visible(false),
        }
        self.progress.set_fraction(view.overall);
        self.line.set_label(&view.progress_line);
        self.button.set_label(view.confirm_label);
        self.button.set_sensitive(view.confirm.enabled);
    }
}

/// A page body of the same shape as `form`, with the heading and the paragraph
/// already built (the recovery page changes them).
fn form_content(
    heading: &gtk::Label,
    intro: &gtk::Label,
    controls: &[&gtk::Widget],
) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.append(heading);
    content.append(intro);
    for control in controls {
        content.append(*control);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(620)
        .tightening_threshold(460)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .child(&content)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build()
}

// --- done ---------------------------------------------------------------------------

pub struct DonePage {
    pub page: adw::NavigationPage,
    status: Closing,
    rows: Rows,
    pub restart: gtk::Button,
    pub power_off: gtk::Button,
    note: gtk::Label,
}

impl DonePage {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let status = Closing::new("object-select-symbolic", "success", 520);
        let rows = Rows::new("", "");
        let restart = pill("Restart");
        restart.add_css_class("suggested-action");
        let power_off = pill("Power off");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::Center);
        buttons.append(&restart);
        buttons.append(&power_off);
        let note = message_label();
        note.set_halign(gtk::Align::Center);
        status.body.append(&rows.group);
        status.body.append(&buttons);
        status.body.append(&note);
        let page = page("Done", "done", None, &status.scroll, None);
        let input = sender.input_sender().clone();
        restart.connect_clicked(move |_| input.emit(Msg::Restart));
        let input = sender.input_sender().clone();
        power_off.connect_clicked(move |_| input.emit(Msg::PowerOff));
        Self {
            page,
            status,
            rows,
            restart,
            power_off,
            note,
        }
    }

    pub fn sync(&mut self, flow: &Flow) {
        let Some(view) = flow.done_view() else {
            return;
        };
        self.status.set(&view.heading, &view.detail);
        let items: Vec<(String, String)> =
            view.rows.iter().map(|r| pair(r.title, &r.value)).collect();
        self.rows.set(&items);
        self.restart.set_sensitive(view.can_finish);
        self.power_off.set_sensitive(view.can_finish);
        show_note(&self.note, view.finishing, view.error.as_ref());
    }
}

/// The line under the buttons: that the computer is going down, or why it is
/// not.
fn show_note(label: &gtk::Label, finishing: Option<&str>, error: Option<&messages::Message>) {
    match (finishing, error) {
        (Some(text), _) => {
            label.set_css_classes(&["caption", "dim-label"]);
            label.set_label(text);
            label.set_visible(true);
        }
        (None, Some(error)) => {
            label.set_css_classes(&["caption", "error"]);
            label.set_label(&sentence(error));
            label.set_visible(true);
        }
        (None, None) => label.set_visible(false),
    }
}

// --- problem ------------------------------------------------------------------------

pub struct ProblemPage {
    pub page: adw::NavigationPage,
    status: Closing,
    log: gtk::TextView,
    log_box: gtk::Box,
    pub retry: gtk::Button,
    pub save: gtk::Button,
    pub restart: gtk::Button,
    note: gtk::Label,
}

impl ProblemPage {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let status = Closing::new("dialog-error-symbolic", "error", 640);
        let (log, scroll) = log_view();
        let caption = gtk::Label::builder()
            .label("The last lines of the log")
            .xalign(0.0)
            .css_classes(["caption-heading"])
            .build();
        let log_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        log_box.append(&caption);
        log_box.append(&scroll);
        let retry = pill("Try again");
        retry.add_css_class("suggested-action");
        let save = pill("Save the log to a USB stick");
        let restart = pill("Restart");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::Center);
        buttons.set_homogeneous(false);
        for button in [&retry, &save, &restart] {
            buttons.append(button);
        }
        let note = message_label();
        note.set_halign(gtk::Align::Center);
        status.body.append(&log_box);
        status.body.append(&buttons);
        status.body.append(&note);
        let page = page("Problem", "problem", None, &status.scroll, None);
        let input = sender.input_sender().clone();
        retry.connect_clicked(move |_| input.emit(Msg::RetryInstall));
        let input = sender.input_sender().clone();
        save.connect_clicked(move |_| input.emit(Msg::ChooseLog));
        let input = sender.input_sender().clone();
        restart.connect_clicked(move |_| input.emit(Msg::Restart));
        Self {
            page,
            status,
            log,
            log_box,
            retry,
            save,
            restart,
            note,
        }
    }

    pub fn sync(&mut self, flow: &Flow) {
        let Some(view) = flow.problem_view() else {
            return;
        };
        self.status.set(&view.message.title, &view.message.detail);
        self.status.set_icon(match view.kind {
            ProblemKind::Lost => "network-error-symbolic",
            ProblemKind::Failed { .. } => "dialog-error-symbolic",
        });
        set_log(&self.log, &view.log);
        self.log_box.set_visible(!view.log.is_empty());
        apply(&self.retry, view.retry);
        apply(&self.save, view.save_log);
        apply(&self.restart, view.restart);
        show_note(&self.note, view.finishing, view.error.as_ref());
    }
}

// --- the log dialog -----------------------------------------------------------------

pub struct LogDialog {
    dialog: adw::Dialog,
    stack: gtk::Stack,
    looking: adw::StatusPage,
    saving: adw::StatusPage,
    saved: adw::StatusPage,
    failed: adw::StatusPage,
    group: adw::PreferencesGroup,
    empty: gtk::Label,
    rows: Vec<adw::ActionRow>,
    shown: Vec<String>,
    presented: bool,
    /// What has the focus in each state, so that Enter does the obvious thing:
    /// the first stick the log can go to, Look again, Close, Try again.
    first_save: Option<gtk::Button>,
    look: gtk::Button,
    close_saved: gtk::Button,
    again: gtk::Button,
}

impl LogDialog {
    pub fn build(sender: &ComponentSender<App>) -> Self {
        let looking = spinning("Looking for USB sticks", "");
        let saving = spinning("Writing the log", "Do not unplug the stick.");
        let saved = status_page("object-select-symbolic");
        let close_saved = pill("Close");
        saved.set_child(Some(&close_saved));
        let failed = status_page("dialog-error-symbolic");
        let again = pill("Try again");
        let close_failed = pill("Close");
        let failed_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        failed_buttons.set_halign(gtk::Align::Center);
        failed_buttons.append(&again);
        failed_buttons.append(&close_failed);
        failed.set_child(Some(&failed_buttons));

        let group = group(
            "Choose where to save the log",
            "The log is a single file with what the installer did: no passwords. It is written to a stick that is not the one the installer started from.",
        );
        let empty = gtk::Label::builder()
            .label("No USB stick or card was found. Plug one in and look again.")
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let look = gtk::Button::with_label("Look again");
        look.set_halign(gtk::Align::End);
        let choosing = gtk::Box::new(gtk::Orientation::Vertical, 12);
        choosing.set_margin_top(18);
        choosing.set_margin_bottom(18);
        choosing.set_margin_start(18);
        choosing.set_margin_end(18);
        choosing.append(&group);
        choosing.append(&empty);
        choosing.append(&look);
        let stack = gtk::Stack::new();
        stack.add_named(&looking, Some("looking"));
        stack.add_named(&choosing, Some("choosing"));
        stack.add_named(&saving, Some("saving"));
        stack.add_named(&saved, Some("saved"));
        stack.add_named(&failed, Some("failed"));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&stack));
        let dialog = adw::Dialog::builder()
            .title("Save the log")
            .content_width(520)
            .child(&toolbar)
            .build();
        let input = sender.input_sender().clone();
        dialog.connect_closed(move |_| input.emit(Msg::CloseLog));
        let input = sender.input_sender().clone();
        look.connect_clicked(move |_| input.emit(Msg::ChooseLog));
        let input = sender.input_sender().clone();
        again.connect_clicked(move |_| input.emit(Msg::ChooseLog));
        let input = sender.input_sender().clone();
        close_failed.connect_clicked(move |_| input.emit(Msg::CloseLog));
        let input = sender.input_sender().clone();
        close_saved.connect_clicked(move |_| input.emit(Msg::CloseLog));
        Self {
            dialog,
            stack,
            looking,
            saving,
            saved,
            failed,
            group,
            empty,
            rows: Vec::new(),
            shown: Vec::new(),
            presented: false,
            first_save: None,
            look,
            close_saved,
            again,
        }
    }

    pub fn is_open(&self) -> bool {
        self.presented
    }

    pub fn present(&mut self, window: &adw::ApplicationWindow) {
        if !self.presented {
            self.dialog.present(Some(window));
            self.presented = true;
        }
    }

    pub fn close(&mut self) {
        self.presented = false;
        self.dialog.force_close();
    }

    pub fn sync(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        let _ = (&self.looking, &self.saving);
        self.dialog
            .set_can_close(!matches!(flow.log_state(), LogSave::Saving { .. }));
        match flow.log_state() {
            LogSave::Closed => {}
            LogSave::Looking => self.stack.set_visible_child_name("looking"),
            LogSave::Choosing(choices) => {
                let key: Vec<String> = choices
                    .iter()
                    .map(|c| format!("{}|{}", c.id, c.problem.is_none()))
                    .collect();
                if key != self.shown {
                    for row in self.rows.drain(..) {
                        self.group.remove(&row);
                    }
                    self.first_save = None;
                    for choice in choices {
                        let subtitle = match &choice.problem {
                            Some(problem) => format!("{}\n{}", choice.subtitle, sentence(problem)),
                            None => choice.subtitle.clone(),
                        };
                        let row = adw::ActionRow::builder()
                            .title(&choice.title)
                            .subtitle(subtitle)
                            .subtitle_lines(0)
                            .build();
                        if choice.problem.is_some() {
                            row.set_sensitive(false);
                        } else {
                            let save = gtk::Button::with_label("Save here");
                            save.add_css_class("suggested-action");
                            save.set_valign(gtk::Align::Center);
                            let id = choice.id.clone();
                            let input = sender.input_sender().clone();
                            save.connect_clicked(move |_| input.emit(Msg::SaveLog(id.clone())));
                            row.add_suffix(&save);
                            row.set_activatable_widget(Some(&save));
                            if self.first_save.is_none() {
                                self.first_save = Some(save);
                            }
                        }
                        self.group.add(&row);
                        self.rows.push(row);
                    }
                    self.shown = key;
                }
                self.group.set_visible(!choices.is_empty());
                self.empty.set_visible(choices.is_empty());
                self.stack.set_visible_child_name("choosing");
                match &self.first_save {
                    Some(save) => self.dialog.set_focus(Some(save)),
                    None => self.dialog.set_focus(Some(&self.look)),
                }
            }
            LogSave::Saving { .. } => self.stack.set_visible_child_name("saving"),
            LogSave::Saved(saved) => {
                let message = messages::log_saved(saved);
                self.saved.set_title(&message.title);
                self.saved.set_description(Some(&message.detail));
                self.stack.set_visible_child_name("saved");
                self.dialog.set_focus(Some(&self.close_saved));
            }
            LogSave::Failed(message) => {
                self.failed.set_title(&message.title);
                self.failed.set_description(Some(&message.detail));
                self.stack.set_visible_child_name("failed");
                self.dialog.set_focus(Some(&self.again));
            }
        }
    }
}
