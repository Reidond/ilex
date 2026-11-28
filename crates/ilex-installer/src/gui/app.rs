//! The application: an `AdwNavigationView` with the welcome, machine, disk,
//! encryption, account, daily user, review, installing, recovery key, done and problem
//! pages, each drawn from the
//! flow's state. The flow decides which
//! page is next and whether Continue works; this file turns its state into
//! widgets and the person's input into flow calls. Nothing here talks to the
//! helper except through the flow's `Backend`, on a worker thread.

use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::events::Event;
pub use ilex_installer_flow::events::Observed;
use ilex_installer_flow::flow::{Flow, PageId, Step};
use ilex_installer_flow::messages;
use ilex_installer_flow::keyboard::SessionKeyboard;
use ilex_installer_flow::runner::{Answer, Services};
use ilex_installer_flow::secret::Secret;
use ilex_installer_flow::view::{self, View};
use relm4::adw::{self, prelude::*};
use relm4::gtk::glib::translate::ToGlibPtr;
use relm4::gtk::{self, glib};
use relm4::prelude::*;

use super::network::NetworkPage;
use super::install::{
    DiskPage, DonePage, InstallingPage, LogDialog, ProblemPage, RecoveryPage, ReviewPage,
    confirmation,
};
use super::profile::ProfilePage;
use crate::serial;

/// Memory is sampled this long after the page is shown, once the session has
/// settled, for the boot report.
const STATS_DELAY: Duration = Duration::from_secs(15);

pub struct Init {
    pub backend: Arc<dyn Backend>,
    pub keyboard: Arc<dyn SessionKeyboard + Send + Sync>,
    /// The flow, with its marker lines on (the real session) or off (the
    /// renderer): what is printed, and when, is the flow's decision
    /// (`Flow::drain_markers`); this application prints what it is handed.
    pub flow: Flow,
    pub fullscreen: bool,
    /// Report memory fifteen seconds after the first page.
    pub stats: bool,
    /// Where the application reports what it shows, for the renderer to wait
    /// on instead of a fixed time.
    pub observer: Option<Arc<Mutex<Observed>>>,
    /// How long the helper is left between two questions about the
    /// installation, instead of what the flow asks (the renderer, so that a
    /// simulated installation does not take its seconds).
    pub poll_ms: Option<u64>,
}

pub struct App {
    flow: Flow,
    services: Services,
    stats: bool,
    observer: Option<Arc<Mutex<Observed>>>,
    mapped: bool,
}

/// What the application is told: an input in the flow's own vocabulary
/// (`ilex_installer_flow::events::Event`: the flow decides what it means), the
/// window being on screen, or (the renderer only) a request to say when
/// everything sent before it has been handled. Secrets travel as `Secret`s:
/// their `Debug` is redacted, so this derived `Debug` prints nothing of a
/// password, and they are wiped when dropped.
#[derive(Debug)]
pub enum Msg {
    Event(Event),
    /// The window is on screen.
    Mapped,
    /// Everything sent before this has been handled when the flag is set
    /// (the renderer only).
    Ping(Arc<AtomicBool>),
}

impl App {
    /// Print the marker lines the flow queued, in its order. Nothing is
    /// decided here: a line the flow did not queue is not printed.
    fn print_markers(&mut self) {
        for line in self.flow.drain_markers() {
            serial::marker(&line);
        }
    }

    /// Carry out what the flow asks for, on a worker: the answer comes back
    /// as a command output carrying the request's generation, and the flow
    /// ignores it if a later request replaced it.
    fn carry_out(&self, step: Step, sender: &ComponentSender<Self>) {
        if let Some(job) = self.services.job(step) {
            sender.spawn_oneshot_command(job);
        }
    }

    /// Tell the renderer what is shown.
    fn observe(&self) {
        if let Some(observer) = &self.observer {
            *observer.lock().unwrap_or_else(|e| e.into_inner()) = self.flow.observed();
        }
    }
}

impl Component for App {
    type Init = Init;
    type Input = Msg;
    type Output = ();
    type CommandOutput = Answer;
    type Root = adw::ApplicationWindow;
    type Widgets = Widgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("Install ilex")
            .default_width(1024)
            .default_height(700)
            .build()
    }

    fn init(init: Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let flow = init.flow;
        let widgets = Widgets::build(&root, &flow, &sender);
        root.set_fullscreened(init.fullscreen);
        if init.fullscreen {
            // The only window of a live session: the compositor's close
            // request (Alt+F4, a window menu) must not end the installer,
            // which would leave tty1 with nothing on it. Only the session
            // unit stops it. `--windowed` (development) closes normally.
            root.connect_close_request(|_| glib::Propagation::Stop);
        }
        let mapped = sender.input_sender().clone();
        root.connect_map(move |_| mapped.emit(Msg::Mapped));
        let mut model = App {
            flow,
            services: Services {
                backend: init.backend,
                keyboard: init.keyboard,
                poll_ms: init.poll_ms,
            },
            stats: init.stats,
            observer: init.observer,
            mapped: false,
        };
        // Where an installation is comes first: an interface that restarted
        // during one comes back on its page. Otherwise the first look at the
        // computer follows.
        let step = model.flow.start();
        model.carry_out(step, &sender);
        model.print_markers();
        model.observe();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Msg, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            Msg::Event(event) => {
                let step = self.flow.handle(event);
                self.carry_out(step, &sender);
            }
            Msg::Mapped => {
                if !self.mapped {
                    self.mapped = true;
                    // The window is on screen: the flow queues the first
                    // page's marker, with the toolkit's own words.
                    let renderer = root
                        .native()
                        .and_then(|native| native.renderer())
                        .map_or_else(|| "none".to_owned(), |r| r.type_().name().to_owned());
                    self.flow.window_shown(&format!(
                        "renderer={renderer} scale={}",
                        root.scale_factor()
                    ));
                    if self.stats {
                        let input = sender.input_sender().clone();
                        glib::timeout_add_local_once(STATS_DELAY, move || {
                            input.emit(Msg::Event(Event::Stats))
                        });
                    }
                }
            }
            Msg::Ping(flag) => flag.store(true, Ordering::SeqCst),
        }
        self.print_markers();
        self.observe();
    }

    fn update_cmd(&mut self, answer: Answer, sender: ComponentSender<Self>, _root: &Self::Root) {
        let step = self.flow.answer(answer);
        self.carry_out(step, &sender);
        self.print_markers();
        self.observe();
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: ComponentSender<Self>) {
        widgets.sync(&self.flow, &sender);
    }
}

// --- widgets ------------------------------------------------------------------

pub struct Widgets {
    root: adw::ApplicationWindow,
    /// A placeholder while the helper has not said where an installation is,
    /// then the pages.
    pages: gtk::Stack,
    nav: adw::NavigationView,
    welcome: Welcome,
    machine: Machine,
    network: NetworkPage,
    disk: DiskPage,
    encryption: Encryption,
    account: Account,
    profile: ProfilePage,
    review: ReviewPage,
    installing: InstallingPage,
    recovery: RecoveryPage,
    done: DonePage,
    problem: ProblemPage,
    /// The erase dialog, while the flow has one open.
    confirm: Option<adw::Dialog>,
    log: LogDialog,
}

struct Welcome {
    banner: adw::Banner,
    language: adw::ComboRow,
    keyboard: adw::ComboRow,
    /// Under the layout list: that the compositor is switching, or why it did
    /// not take the layout.
    keyboard_message: gtk::Label,
    keyboard_retry: gtk::Button,
    keyboard_line: gtk::Box,
    button: gtk::Button,
}

struct Machine {
    page: adw::NavigationPage,
    banner: adw::Banner,
    stack: gtk::Stack,
    problem: adw::StatusPage,
    computer: adw::ActionRow,
    group: adw::PreferencesGroup,
    notice: gtk::Label,
    rows: Vec<(String, gtk::CheckButton, adw::ActionRow)>,
    button: gtk::Button,
}

struct Encryption {
    page: adw::NavigationPage,
    banner: adw::Banner,
    switch: adw::SwitchRow,
    /// Unlock with the security chip, where the computer can.
    chip: adw::SwitchRow,
    /// The passphrase and its confirmation, in a group that is shown while
    /// encryption is on.
    group: adw::PreferencesGroup,
    passphrase: adw::PasswordEntryRow,
    confirmation: adw::PasswordEntryRow,
    /// Under the fields: the keyboard layout the passphrase is typed in.
    layout: gtk::Label,
    message: gtk::Label,
    /// While encryption is off: what that means.
    off: gtk::Label,
    button: gtk::Button,
    /// The flow's wipe count the fields were last cleared at.
    wiped: u64,
}

struct Account {
    page: adw::NavigationPage,
    banner: adw::Banner,
    name: adw::EntryRow,
    password: adw::PasswordEntryRow,
    confirmation: adw::PasswordEntryRow,
    name_message: gtk::Label,
    password_message: gtk::Label,
    button: gtk::Button,
    /// The flow's wipe count the fields were last cleared at.
    wiped: u64,
}

/// A form page's body: a heading and a paragraph, then the controls, centred
/// and scrollable.
pub(super) fn form(heading: &str, text: &str, controls: &[&gtk::Widget]) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let title = gtk::Label::builder()
        .label(heading)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-1"])
        .build();
    let body = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["body"])
        .build();
    content.append(&title);
    content.append(&body);
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

pub(super) fn page(
    title: &str,
    tag: &str,
    banner: Option<&adw::Banner>,
    content: &impl IsA<gtk::Widget>,
    button: Option<&gtk::Button>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    // The installer is the only window of its session and cannot be closed:
    // no window buttons.
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    toolbar.add_top_bar(&header);
    if let Some(banner) = banner {
        toolbar.add_top_bar(banner);
    }
    toolbar.set_content(Some(content));
    if let Some(button) = button {
        let bar = gtk::ActionBar::new();
        bar.pack_end(button);
        toolbar.add_bottom_bar(&bar);
    }
    adw::NavigationPage::builder()
        .title(title)
        .tag(tag)
        .child(&toolbar)
        .build()
}

pub(super) fn continue_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("suggested-action");
    button.set_margin_top(6);
    button.set_margin_bottom(6);
    button
}

pub(super) fn banner(button: Option<&str>) -> adw::Banner {
    let banner = adw::Banner::new("");
    banner.set_button_label(button);
    banner
}

pub(super) fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    if !title.is_empty() {
        group.set_title(title);
    }
    if !description.is_empty() {
        group.set_description(Some(description));
    }
    group
}

pub(super) fn message_label() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption"])
        .visible(false)
        .build()
}

/// A combo row with a searchable list: the layouts alone number several hundred.
fn combo(title: &str, items: &[String]) -> adw::ComboRow {
    let strings: Vec<&str> = items.iter().map(String::as_str).collect();
    let row = adw::ComboRow::new();
    row.set_title(title);
    row.set_model(Some(&gtk::StringList::new(&strings)));
    row.set_enable_search(true);
    row.set_expression(Some(&gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<gtk::Expression>,
        "string",
    )));
    row
}

pub(super) fn property_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle_selectable(true)
        .build();
    row.add_css_class("property");
    row
}

impl Widgets {
    fn build(root: &adw::ApplicationWindow, flow: &Flow, sender: &ComponentSender<App>) -> Self {
        let nav = adw::NavigationView::new();
        let starting = adw::StatusPage::builder()
            .title(view::STARTING_PAGE)
            .child(&adw::Spinner::new())
            .build();
        let pages = gtk::Stack::new();
        pages.add_named(&starting, Some("starting"));
        pages.add_named(&nav, Some("pages"));
        root.set_content(Some(&pages));

        // --- welcome ---
        let catalogue = flow.catalogue();
        let language = combo(view::LANGUAGE_LABEL, &catalogue.languages);
        let keyboard = combo(view::KEYBOARD_LABEL, &catalogue.layouts);
        let test = adw::EntryRow::builder()
            .title(view::TRY_LAYOUT_LABEL)
            .build();
        let welcome_banner = banner(None);
        let welcome_button = continue_button(messages::CONTINUE_LABEL);
        let language_group = group("", view::LANGUAGE_HELP);
        language_group.add(&language);
        let keyboard_group = group("", view::KEYBOARD_HELP);
        keyboard_group.add(&keyboard);
        keyboard_group.add(&test);
        let keyboard_message = message_label();
        keyboard_message.set_hexpand(true);
        let keyboard_retry = gtk::Button::with_label(view::TRY_AGAIN);
        keyboard_retry.add_css_class("flat");
        keyboard_retry.set_valign(gtk::Align::Center);
        keyboard_retry.set_visible(false);
        let keyboard_line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        keyboard_line.append(&keyboard_message);
        keyboard_line.append(&keyboard_retry);
        keyboard_line.set_visible(false);
        let welcome_form = form(
            view::WELCOME_HEADING,
            view::WELCOME_INTRO,
            &[
                language_group.upcast_ref(),
                keyboard_group.upcast_ref(),
                keyboard_line.upcast_ref(),
            ],
        );
        let welcome_page = page(
            view::WELCOME_TITLE,
            PageId::Welcome.id(),
            Some(&welcome_banner),
            &welcome_form,
            Some(&welcome_button),
        );

        // --- machine ---
        let machine_banner = banner(None);
        let computer = property_row(view::COMPUTER_LABEL);
        let machine_group = group(view::CONFIGURATION, "");
        let notice = message_label();
        let machine_button = continue_button(messages::CONTINUE_LABEL);
        let loading = adw::StatusPage::builder()
            .title(view::MACHINE_LOADING_TITLE)
            .description(view::MACHINE_LOADING_DETAIL)
            .child(&adw::Spinner::new())
            .build();
        let problem = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .build();
        let retry = gtk::Button::with_label(view::TRY_AGAIN);
        retry.add_css_class("pill");
        retry.set_halign(gtk::Align::Center);
        problem.set_child(Some(&retry));
        let computer_group = group("", "");
        computer_group.add(&computer);
        let machine_body = gtk::Box::new(gtk::Orientation::Vertical, 18);
        machine_body.append(&computer_group);
        machine_body.append(&notice);
        machine_body.append(&machine_group);
        let stack = gtk::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&problem, Some("problem"));
        let machine_form = form(
            view::MACHINE_HEADING,
            view::MACHINE_INTRO,
            &[machine_body.upcast_ref()],
        );
        stack.add_named(&machine_form, Some("choices"));
        let machine_page = page(
            view::MACHINE_TITLE,
            PageId::Machine.id(),
            Some(&machine_banner),
            &stack,
            Some(&machine_button),
        );
        let on_retry = sender.input_sender().clone();
        retry.connect_clicked(move |_| on_retry.emit(Msg::Event(Event::Probe)));

        // --- account ---
        let name = adw::EntryRow::builder().title(view::NAME_LABEL).build();
        name.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
        let password = adw::PasswordEntryRow::builder()
            .title(view::PASSWORD_LABEL)
            .name("password")
            .build();
        let confirmation = adw::PasswordEntryRow::builder()
            .title(view::CONFIRMATION_LABEL)
            .name("confirmation")
            .build();
        // GtkText keeps an undo history of what is typed, unwiped, and with
        // "show" on it holds the visible text too: nothing a person types
        // into these fields is kept for Ctrl+Z.
        test.set_enable_undo(false);
        password.set_enable_undo(false);
        confirmation.set_enable_undo(false);
        let name_message = message_label();
        let password_message = message_label();
        let name_group = group("", "");
        name_group.add(&name);
        let password_group = group("", "");
        password_group.add(&password);
        password_group.add(&confirmation);
        let account_button = continue_button(messages::CONTINUE_LABEL);
        let account_form = form(
            view::ACCOUNT_HEADING,
            view::ACCOUNT_INTRO,
            &[
                name_group.upcast_ref(),
                name_message.upcast_ref(),
                password_group.upcast_ref(),
                password_message.upcast_ref(),
            ],
        );
        let account_banner = banner(None);
        let account_page = page(
            view::ACCOUNT_TITLE,
            PageId::Account.id(),
            Some(&account_banner),
            &account_form,
            Some(&account_button),
        );

        // --- encryption ---
        let switch = adw::SwitchRow::builder()
            .title(view::ENCRYPT_TITLE)
            .subtitle(view::ENCRYPT_SUBTITLE)
            .name("encrypt")
            .active(flow.encrypt())
            .build();
        let passphrase = adw::PasswordEntryRow::builder()
            .title(view::PASSPHRASE_LABEL)
            .name("passphrase")
            .build();
        let passphrase_confirmation = adw::PasswordEntryRow::builder()
            .title(view::PASSPHRASE_CONFIRMATION_LABEL)
            .name("passphrase-confirmation")
            .build();
        // As the account page's: nothing a person types is kept for Ctrl+Z.
        passphrase.set_enable_undo(false);
        passphrase_confirmation.set_enable_undo(false);
        let encrypt_group = group("", "");
        encrypt_group.add(&switch);
        // The security chip: on where the computer can, with one line that says
        // why not where it cannot (`view.encryption.chip`).
        let chip = adw::SwitchRow::builder()
            .title(ilex_installer_flow::recovery::CHIP_TITLE)
            .subtitle_lines(0)
            .name("chip")
            .active(false)
            .sensitive(false)
            .build();
        encrypt_group.add(&chip);
        let passphrase_group = group("", "");
        passphrase_group.add(&passphrase);
        passphrase_group.add(&passphrase_confirmation);
        let passphrase_layout = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        let passphrase_message = message_label();
        let encryption_off = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .label(ilex_installer_flow::encryption::OFF_LINE)
            .css_classes(["caption", "warning"])
            .visible(false)
            .build();
        let encryption_button = continue_button(messages::CONTINUE_LABEL);
        let encryption_form = form(
            view::ENCRYPTION_HEADING,
            ilex_installer_flow::encryption::INTRODUCTION,
            &[
                encrypt_group.upcast_ref(),
                passphrase_group.upcast_ref(),
                passphrase_layout.upcast_ref(),
                passphrase_message.upcast_ref(),
                encryption_off.upcast_ref(),
            ],
        );
        let encryption_banner = banner(None);
        let encryption_page = page(
            view::ENCRYPTION_TITLE,
            PageId::Encryption.id(),
            Some(&encryption_banner),
            &encryption_form,
            Some(&encryption_button),
        );

        // --- disk, review, installing, recovery key, done, problem ---
        let network = NetworkPage::build(sender, flow.secrets_wiped());
        let disk = DiskPage::build(sender);
        let profile = ProfilePage::build(sender);
        let review = ReviewPage::build(sender);
        let installing = InstallingPage::build();
        let recovery = RecoveryPage::build(sender, flow.secrets_wiped());
        let done = DonePage::build(sender);
        let problem_page = ProblemPage::build(sender);
        let log = LogDialog::build(sender);

        for page in [
            &welcome_page,
            &network.page,
            &machine_page,
            &disk.page,
            &encryption_page,
            &account_page,
            &profile.page,
            &review.page,
            &installing.page,
            &recovery.page,
            &done.page,
            &problem_page.page,
        ] {
            nav.add(page);
        }
        nav.set_pop_on_escape(true);

        // --- input ---
        let input = sender.input_sender().clone();
        language.connect_selected_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Language(row.selected() as usize)))
        });
        keyboard.connect_selected_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Layout(row.selected() as usize)))
        });
        // The layout test field is read only when the flow says its text may be
        // printed (a test): people type their intended passwords there.
        if flow.echo_try() {
            test.connect_changed({
                let input = input.clone();
                move |row| input.emit(Msg::Event(Event::TryText(secret_of(row))))
            });
        }
        keyboard_retry.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Event(Event::RetryLayout))
        });
        name.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Name(row.text().to_string())))
        });
        password.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Password(secret_of(row))))
        });
        confirmation.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Confirmation(secret_of(row))))
        });
        switch.connect_active_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Encrypt(row.is_active())))
        });
        chip.connect_active_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Chip(row.is_active())))
        });
        passphrase.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::Passphrase(secret_of(row))))
        });
        passphrase_confirmation.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Event(Event::PassphraseConfirmation(secret_of(row))))
        });
        for banner in [
            &welcome_banner,
            &machine_banner,
            &encryption_banner,
            &account_banner,
        ] {
            banner.connect_button_clicked({
                let input = input.clone();
                move |_| input.emit(Msg::Event(Event::Probe))
            });
        }
        for button in [
            &welcome_button,
            &machine_button,
            &encryption_button,
            &account_button,
        ] {
            button.connect_clicked({
                let input = input.clone();
                move |_| input.emit(Msg::Event(Event::Continue))
            });
        }
        // Back (the header bar's button or Escape) pops the page: the flow
        // follows the view.
        nav.connect_popped({
            let input = input.clone();
            move |view, _| {
                if let Some(page) = view
                    .visible_page()
                    .and_then(|p| p.tag())
                    .and_then(|t| PageId::from_id(&t))
                {
                    input.emit(Msg::Event(Event::Visible(page)));
                }
            }
        });
        // Enter in a text field continues when the page is valid (the flow
        // refuses when it is not). Enter elsewhere keeps its own meaning: it
        // opens a list, toggles a row, presses the focused button. A window
        // default widget would take Enter from the combo rows, so there is none.
        for row in [
            test.upcast_ref::<adw::EntryRow>(),
            name.upcast_ref(),
            password.upcast_ref(),
            confirmation.upcast_ref(),
            passphrase.upcast_ref(),
            passphrase_confirmation.upcast_ref(),
        ] {
            row.connect_entry_activated({
                let input = input.clone();
                move |_| input.emit(Msg::Event(Event::Continue))
            });
        }
        // The first control of each page has the focus when the page is shown.
        for (page, focus) in [
            (&welcome_page, language.clone().upcast::<gtk::Widget>()),
            (&machine_page, machine_button.clone().upcast()),
            (&disk.page, disk.button.clone().upcast()),
            (&account_page, name.clone().upcast()),
            (&profile.page, profile.button.clone().upcast()),
            (&review.page, review.button.clone().upcast()),
            (&done.page, done.restart.clone().upcast()),
        ] {
            page.connect_shown(move |_| {
                focus.grab_focus();
            });
        }

        // The encryption page starts on the passphrase while encryption is on
        // (what nearly everyone does first), else on the switch.
        encryption_page.connect_shown({
            let switch = switch.clone();
            let passphrase = passphrase.clone();
            move |_| {
                if switch.is_active() {
                    passphrase.grab_focus();
                } else {
                    switch.grab_focus();
                }
            }
        });

        // The problem page starts on Try again when it has one, else on Restart.
        problem_page.page.connect_shown({
            let retry = problem_page.retry.clone();
            let restart = problem_page.restart.clone();
            move |_| {
                if retry.is_visible() && retry.is_sensitive() {
                    retry.grab_focus();
                } else {
                    restart.grab_focus();
                }
            }
        });

        // `add` shows the first page of an empty view before the handler above
        // exists: GTK's own first focus (the first row) stays.

        Self {
            root: root.clone(),
            pages,
            nav,
            welcome: Welcome {
                banner: welcome_banner,
                language,
                keyboard,
                keyboard_message,
                keyboard_retry,
                keyboard_line,
                button: welcome_button,
            },
            machine: Machine {
                page: machine_page.clone(),
                banner: machine_banner,
                stack,
                problem,
                computer,
                group: machine_group,
                notice,
                rows: Vec::new(),
                button: machine_button,
            },
            encryption: Encryption {
                page: encryption_page,
                banner: encryption_banner,
                switch,
                chip,
                group: passphrase_group,
                passphrase,
                confirmation: passphrase_confirmation,
                layout: passphrase_layout,
                message: passphrase_message,
                off: encryption_off,
                button: encryption_button,
                wiped: flow.secrets_wiped(),
            },
            account: Account {
                page: account_page,
                banner: account_banner,
                name,
                password,
                confirmation,
                name_message,
                password_message,
                button: account_button,
                wiped: flow.secrets_wiped(),
            },
            network,
            disk,
            profile,
            review,
            installing,
            recovery,
            done,
            problem: problem_page,
            confirm: None,
            log,
        }
    }

    /// Make every widget say what the flow says (`Flow::view`).
    fn sync(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        let v = flow.view();
        // Nothing of the pages until the helper has said where an
        // installation is (the answer takes a moment at start-up).
        self.pages.set_visible_child_name(if v.starting.is_some() {
            "starting"
        } else {
            "pages"
        });
        // Which page: the navigation view follows the flow (a page pushed by
        // Continue), and the flow follows the view when the person goes back.
        if v.starting.is_none() {
            show_stack(&self.nav, &v.stack);
        }
        self.sync_welcome(&v);
        self.sync_machine(&v, sender);
        self.network.sync(&v, sender);
        self.disk.sync(&v, sender);
        self.sync_encryption(&v);
        self.sync_account(&v, flow);
        self.profile.sync(&v, sender);
        self.review.sync(&v, flow);
        self.installing.sync(&v);
        self.recovery.sync(&v);
        self.done.sync(&v);
        self.problem.sync(&v);
        // Whether the person may go back from a page is the flow's to say.
        self.machine.page.set_can_pop(v.machine.chrome.can_pop);
        self.disk.page.set_can_pop(v.disk.chrome.can_pop);
        self.encryption
            .page
            .set_can_pop(v.encryption.chrome.can_pop);
        self.account.page.set_can_pop(v.account.chrome.can_pop);
        self.profile.page.set_can_pop(v.profile.chrome.can_pop);
        self.review.page.set_can_pop(v.review.chrome.can_pop);
        for page in [
            &self.installing.page,
            &self.recovery.page,
            &self.done.page,
            &self.problem.page,
        ] {
            page.set_can_pop(false);
        }
        // The dialogs are open exactly while the flow says so.
        match (&v.confirm, &self.confirm) {
            (Some(dialog), None) => {
                let alert = confirmation(dialog, sender);
                alert.present(Some(&self.root));
                self.confirm = Some(alert);
            }
            (None, Some(_)) => {
                if let Some(alert) = self.confirm.take() {
                    alert.force_close();
                }
                // The destructive button has the focus again (a cancelled
                // dialog leaves the page as it was).
                if flow.page() == PageId::Review {
                    self.review.button.grab_focus();
                }
            }
            _ => {}
        }
        match &v.log {
            None => {
                if self.log.is_open() {
                    self.log.close();
                    // Back on the button that opened it.
                    self.problem.save.grab_focus();
                }
            }
            Some(_) => {
                self.log.sync(&v, sender);
                self.log.present(&self.root);
            }
        }
    }

    fn sync_welcome(&mut self, v: &View) {
        let w = &self.welcome;
        let welcome = &v.welcome;
        sync_banner(&w.banner, &welcome.chrome.banner);
        if let Some(index) = welcome.language.selected
            && w.language.selected() != index as u32
        {
            w.language.set_selected(index as u32);
        }
        if let Some(index) = welcome.keyboard.selected
            && w.keyboard.selected() != index as u32
        {
            w.keyboard.set_selected(index as u32);
        }
        match &welcome.keyboard_line {
            None => w.keyboard_line.set_visible(false),
            Some(line) => {
                w.keyboard_message.set_css_classes(&[
                    "caption",
                    if line.failed { "error" } else { "dim-label" },
                ]);
                w.keyboard_message.set_label(&line.text);
                w.keyboard_message.set_visible(true);
                w.keyboard_retry.set_visible(line.retry.is_some());
                w.keyboard_line.set_visible(true);
            }
        }
        if let Some(next) = &welcome.chrome.next {
            w.button.set_label(next.label);
            w.button.set_sensitive(next.enabled);
        }
    }

    fn sync_machine(&mut self, v: &View, sender: &ComponentSender<App>) {
        let m = &mut self.machine;
        let machine = &v.machine;
        if let Some(next) = &machine.chrome.next {
            m.button.set_label(next.label);
            m.button.set_sensitive(next.enabled);
        }
        sync_banner(&m.banner, &machine.chrome.banner);
        match &machine.body {
            view::MachineBody::Loading { .. } => m.stack.set_visible_child_name("loading"),
            view::MachineBody::Problem { title, detail, .. } => {
                m.problem.set_title(title);
                m.problem.set_description(Some(detail));
                m.stack.set_visible_child_name("problem");
            }
            view::MachineBody::Choices {
                computer,
                choices,
                selected,
                notice,
                ..
            } => {
                m.stack.set_visible_child_name("choices");
                m.computer.set_subtitle(computer);
                rebuild_choices(m, choices, sender);
                for (tag, check, _) in &m.rows {
                    if check.is_active() != (tag == selected) {
                        check.set_active(tag == selected);
                    }
                }
                // How the configuration was chosen: in the page when the
                // banner is saying something else.
                m.notice.set_visible(notice.is_some());
                if let Some(text) = notice {
                    m.notice.set_label(text);
                }
            }
        }
    }

    fn sync_encryption(&mut self, v: &View) {
        let encryption = &v.encryption;
        // The flow wiped its copy of the passphrase (the installation is over,
        // or the computer is going down): the fields that hold it are cleared
        // too.
        if self.encryption.wiped != encryption.wiped {
            self.encryption.wiped = encryption.wiped;
            self.encryption.passphrase.set_text("");
            self.encryption.confirmation.set_text("");
        }
        let e = &self.encryption;
        sync_banner(&e.banner, &encryption.chrome.banner);
        // The switch follows the flow (a restarted interface, the renderer);
        // the flow ignores an echo of its own value.
        if e.switch.is_active() != encryption.encrypt.on {
            e.switch.set_active(encryption.encrypt.on);
        }
        // The choices cannot change while the helper checks them (or while it is
        // asked where an installation is, which is how the application starts).
        // A field of this page that has the focus gives it up first (GTK warns
        // when a focused text becomes insensitive); one that has it elsewhere
        // keeps it: the welcome page's first control has it at the start.
        if e.switch.is_sensitive() != encryption.encrypt.enabled {
            if !encryption.encrypt.enabled {
                release_focus(&e.page);
            }
            e.switch.set_sensitive(encryption.encrypt.enabled);
        }
        // The security chip's row: a switch where the computer can, else a line
        // that says why it cannot.
        let chip = &encryption.chip;
        if e.chip.is_sensitive() != chip.enabled {
            if !chip.enabled {
                release_focus(&e.page);
            }
            e.chip.set_sensitive(chip.enabled);
        }
        if e.chip.is_active() != chip.on {
            e.chip.set_active(chip.on);
        }
        if e.chip.subtitle().is_none_or(|s| s.as_str() != chip.line) {
            e.chip.set_subtitle(&chip.line);
        }
        e.group.set_visible(encryption.fields);
        e.layout.set_visible(encryption.fields);
        e.off.set_visible(encryption.off_line.is_some());
        if e.passphrase.is_sensitive() != encryption.editable {
            if !encryption.editable {
                release_focus(&e.page);
            }
            e.passphrase.set_sensitive(encryption.editable);
            e.confirmation.set_sensitive(encryption.editable);
        }
        e.layout.set_label(&encryption.layout_line);
        let rows: [&gtk::Widget; 2] = [e.passphrase.upcast_ref(), e.confirmation.upcast_ref()];
        show_note(&e.message, encryption.note.as_ref(), &rows);
        if let Some(next) = &encryption.chrome.next {
            e.button.set_label(next.label);
            e.button.set_sensitive(next.enabled);
        }
    }

    fn sync_account(&mut self, v: &View, flow: &Flow) {
        let account = &v.account;
        // The flow wiped its copy of the passwords (the installation is over,
        // or the computer is going down): the fields that hold them are
        // cleared too, not left with the text.
        if self.account.wiped != account.wiped {
            self.account.wiped = account.wiped;
            self.account.password.set_text("");
            self.account.confirmation.set_text("");
        }
        let a = &self.account;
        sync_banner(&a.banner, &account.chrome.banner);
        // The choices cannot change while the helper checks them. A field that
        // has the focus gives it up first (GTK warns when a focused text
        // becomes insensitive), and the name has it again when the page is
        // editable once more.
        let editable = account.editable;
        if a.name.is_sensitive() != editable {
            if editable {
                a.name.set_sensitive(true);
                a.password.set_sensitive(true);
                a.confirmation.set_sensitive(true);
                if flow.page() == PageId::Account {
                    a.name.grab_focus();
                }
            } else {
                if let Some(root) = a.page.root() {
                    root.set_focus(None::<&gtk::Widget>);
                }
                a.name.set_sensitive(false);
                a.password.set_sensitive(false);
                a.confirmation.set_sensitive(false);
            }
        }
        // The name is not private and may come from the flow (the renderer
        // sends it as a message). The passwords never do: the fields are
        // where they are typed, and the flow takes a copy as they change;
        // reading them here would copy them again on every update.
        if a.name.text() != account.name_value {
            a.name.set_text(&account.name_value);
        }
        show_note(
            &a.name_message,
            account.name_note.as_ref(),
            &[a.name.upcast_ref()],
        );
        let rows: [&gtk::Widget; 2] = [a.password.upcast_ref(), a.confirmation.upcast_ref()];
        show_note(&a.password_message, account.password_note.as_ref(), &rows);
        if let Some(next) = &account.chrome.next {
            a.button.set_label(next.label);
            a.button.set_sensitive(next.enabled);
        }
    }
}

/// The focus leaves `page` when something on it has it (GTK warns when a
/// focused widget becomes insensitive).
fn release_focus(page: &adw::NavigationPage) {
    if let Some(root) = page.root()
        && let Some(focus) = root.focus()
        && focus.is_ancestor(page)
    {
        root.set_focus(None::<&gtk::Widget>);
    }
}

/// Show a note beside its field, or hide the label. A refusal (an error note)
/// also marks the rows: the error style, and for assistive technology the
/// rows' invalid state with the message as their error message; advice is
/// dimmed text and the rows go back to valid (state and relation included), as
/// they do with no note at all.
pub(super) fn show_note(label: &gtk::Label, note: Option<&view::Note>, rows: &[&gtk::Widget]) {
    match note {
        Some(note) => {
            label.set_css_classes(&["caption", if note.error { "error" } else { "dim-label" }]);
            label.set_label(&note.text);
            label.set_visible(true);
        }
        None => label.set_visible(false),
    }
    let refused = note.is_some_and(|n| n.error);
    for row in rows {
        if refused {
            row.add_css_class("error");
            row.update_state(&[gtk::accessible::State::Invalid(
                gtk::AccessibleInvalidState::True,
            )]);
            row.update_relation(&[gtk::accessible::Relation::ErrorMessage(&[label
                .upcast_ref::<gtk::Accessible>(
            )])]);
        } else {
            row.remove_css_class("error");
            row.update_state(&[gtk::accessible::State::Invalid(
                gtk::AccessibleInvalidState::False,
            )]);
            row.reset_relation(gtk::AccessibleRelation::ErrorMessage);
        }
    }
}

/// A page's banner as the flow says it: revealed with its text and button when
/// there is one, hidden when there is none.
pub(super) fn sync_banner(banner: &adw::Banner, say: &Option<view::Banner>) {
    banner.set_revealed(say.is_some());
    if let Some(say) = say {
        banner.set_title(&say.text);
        banner.set_button_label(say.retry);
    }
}

/// The tags of the pages the navigation view holds, bottom first.
fn stack_tags(nav: &adw::NavigationView) -> Vec<String> {
    let stack = nav.navigation_stack();
    (0..stack.n_items())
        .filter_map(|i| stack.item(i).and_downcast::<adw::NavigationPage>())
        .filter_map(|page| page.tag().map(|tag| tag.to_string()))
        .collect()
}

/// Make the navigation view hold exactly `want` (the flow's stack for its page):
/// one step on from what it holds is a push, one step back a pop, anything
/// else (the flow moved between the pages an installation ends on, which have
/// nothing under them, or from the review page to the installing page) replaces
/// the stack. A page the view does not hold is never popped to.
fn show_stack(nav: &adw::NavigationView, want: &[&str]) {
    let want: Vec<&str> = want.to_vec();
    let have = stack_tags(nav);
    if have.iter().map(String::as_str).eq(want.iter().copied()) {
        return;
    }
    let held = |tags: &[&str]| -> bool {
        tags.len() == have.len() && tags.iter().zip(&have).all(|(wanted, held)| wanted == held)
    };
    let pushed = want.len() == have.len() + 1 && held(&want[..have.len()]);
    let popped =
        want.len() < have.len() && have.iter().zip(&want).all(|(held, wanted)| held == wanted);
    match want.last() {
        Some(last) if pushed => nav.push_by_tag(last),
        Some(last) if popped && nav.pop_to_tag(last) => {}
        _ => nav.replace_with_tags(&want),
    }
}

/// The text of a field, copied once from the toolkit's own buffer straight
/// into a `Secret`: no `String` or `GString` of it is made, so the only
/// copies are the field's (secure) buffer and a value that is wiped when it is
/// dropped.
pub(super) fn secret_of(row: &impl IsA<gtk::Editable>) -> Secret {
    let editable: &gtk::Editable = row.upcast_ref();
    // SAFETY: `gtk_editable_get_text` returns a NUL-terminated string that
    // the widget owns and keeps valid until its text changes; it is only read
    // here, before anything can change it.
    unsafe {
        let text = gtk::ffi::gtk_editable_get_text(editable.to_glib_none().0);
        if text.is_null() {
            return Secret::default();
        }
        Secret::from_utf8(CStr::from_ptr(text).to_bytes()).unwrap_or_default()
    }
}

/// One radio row per configuration, rebuilt only when the list changes.
fn rebuild_choices(m: &mut Machine, choices: &[view::Choice], sender: &ComponentSender<App>) {
    let tags: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
    if m.rows
        .iter()
        .map(|(t, _, _)| t.as_str())
        .collect::<Vec<_>>()
        == tags
    {
        return;
    }
    for (_, _, row) in m.rows.drain(..) {
        m.group.remove(&row);
    }
    let mut first: Option<gtk::CheckButton> = None;
    for choice in choices {
        let check = gtk::CheckButton::new();
        if let Some(first) = &first {
            check.set_group(Some(first));
        } else {
            first = Some(check.clone());
        }
        let row = adw::ActionRow::builder()
            .title(&choice.title)
            .subtitle(&choice.subtitle)
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        let tag = choice.id.clone();
        let input = sender.input_sender().clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                input.emit(Msg::Event(Event::Machine(tag.clone())));
            }
        });
        m.group.add(&row);
        m.rows.push((choice.id.clone(), check, row));
    }
}
