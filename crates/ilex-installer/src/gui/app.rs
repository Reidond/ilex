//! The application: an `AdwNavigationView` with the welcome, machine, account
//! and ready pages, each drawn from the flow's state. The flow decides which
//! page is next and whether Continue works; this file turns its state into
//! widgets and the person's input into flow calls. Nothing here talks to the
//! helper except through the flow's `Backend`, on a worker thread.

use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ilex_installer_flow::Message;
use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::flow::{
    Flow, KeyboardStatus, PageId, PlanCheck, ProbeState, Step, human_bytes,
};
use ilex_installer_flow::keyboard::SessionKeyboard;
use ilex_installer_flow::language::{self, LANGUAGES};
use ilex_installer_flow::machine::{MachineView, Severity};
use ilex_installer_flow::messages::{self, ClientError};
use ilex_installer_flow::secret::Secret;
use ilex_installer_model::plan::Plan;
use ilex_installer_model::probe::Probe;
use relm4::adw::{self, prelude::*};
use relm4::gtk::glib::translate::ToGlibPtr;
use relm4::gtk::{self, glib};
use relm4::prelude::*;

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
}

/// What the application is showing, in the flow's words.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    pub page: PageId,
    /// `Flow::check_word`: `not-run`, `running`, `ok`, `no-disk`, `failed`.
    pub check: &'static str,
    pub probe_pending: bool,
    pub keyboard_switching: bool,
}

impl Observed {
    /// Nothing is under way: no probe, no check, no layout switch.
    pub fn settled(&self) -> bool {
        self.check != "running" && !self.probe_pending && !self.keyboard_switching
    }
}

pub struct App {
    flow: Flow,
    backend: Arc<dyn Backend>,
    keyboard: Arc<dyn SessionKeyboard + Send + Sync>,
    stats: bool,
    observer: Option<Arc<Mutex<Observed>>>,
    mapped: bool,
}

/// Secrets travel as `Secret`s: their `Debug` is redacted, so this derived
/// `Debug` prints nothing of a password, and they are wiped when dropped.
#[derive(Debug)]
pub enum Msg {
    /// Ask the helper about this computer (again).
    Probe,
    Language(u32),
    Layout(u32),
    /// Try the layout again after the compositor did not take it.
    RetryLayout,
    TryText(Secret),
    Machine(String),
    Name(String),
    Password(Secret),
    Confirmation(Secret),
    Continue,
    /// The closing page's Try again: look again, then check the choices.
    Retry,
    /// The navigation view now shows this page (the person went back).
    Visible(PageId),
    /// The window is on screen.
    Mapped,
    /// Show a page directly (the renderer only).
    Jump(PageId),
    /// The session has settled: report memory.
    Stats,
    /// Everything sent before this has been handled when the flag is set
    /// (the renderer only).
    Ping(Arc<AtomicBool>),
}

#[derive(Debug)]
pub enum Cmd {
    Probed {
        generation: u64,
        result: Result<Probe, ClientError>,
    },
    Planned {
        generation: u64,
        result: Result<Plan, ClientError>,
    },
    Switched {
        generation: u64,
        layout: String,
        result: Result<(), String>,
    },
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
        match step {
            Step::RunProbe { generation } => {
                let backend = Arc::clone(&self.backend);
                sender.spawn_oneshot_command(move || Cmd::Probed {
                    generation,
                    result: backend.probe(),
                });
            }
            Step::RunPlan {
                generation,
                request,
            } => {
                let backend = Arc::clone(&self.backend);
                sender.spawn_oneshot_command(move || Cmd::Planned {
                    generation,
                    result: backend.plan(&request),
                });
            }
            Step::SwitchKeyboard { generation, layout } => {
                // The session types in the new layout from now on; the call
                // blocks on D-Bus. The flow keeps one switch outstanding, so
                // the compositor ends on the last layout asked for.
                let keyboard = Arc::clone(&self.keyboard);
                sender.spawn_oneshot_command(move || Cmd::Switched {
                    generation,
                    layout: layout.id.clone(),
                    result: keyboard.switch(&layout).map_err(|e| format!("{e:#}")),
                });
            }
            // The widgets follow the flow's page.
            Step::Goto(_) | Step::Stay => {}
        }
    }

    /// Tell the renderer what is shown.
    fn observe(&self) {
        if let Some(observer) = &self.observer {
            let now = Observed {
                page: self.flow.page(),
                check: self.flow.check_word(),
                probe_pending: matches!(self.flow.probe_state(), ProbeState::Pending),
                keyboard_switching: self.flow.keyboard_status() == KeyboardStatus::Switching,
            };
            *observer.lock().unwrap_or_else(|e| e.into_inner()) = now;
        }
    }
}

impl Component for App {
    type Init = Init;
    type Input = Msg;
    type Output = ();
    type CommandOutput = Cmd;
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
            backend: init.backend,
            keyboard: init.keyboard,
            stats: init.stats,
            observer: init.observer,
            mapped: false,
        };
        let step = model.flow.start_probe();
        model.carry_out(step, &sender);
        model.print_markers();
        model.observe();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Msg, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            Msg::Probe => {
                let step = self.flow.start_probe();
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
                        glib::timeout_add_local_once(STATS_DELAY, move || input.emit(Msg::Stats));
                    }
                }
            }
            Msg::Language(index) => {
                if let Some(language) = LANGUAGES.get(index as usize) {
                    self.flow.set_language(language.locale);
                }
            }
            Msg::Layout(index) => {
                let id = self
                    .flow
                    .layouts()
                    .all()
                    .get(index as usize)
                    .map(|l| l.id.clone());
                if let Some(id) = id
                    && self.flow.keyboard() != id
                {
                    let step = self.flow.set_keyboard(&id);
                    self.carry_out(step, &sender);
                }
            }
            Msg::RetryLayout => {
                let step = self.flow.retry_keyboard();
                self.carry_out(step, &sender);
            }
            Msg::TryText(text) => self.flow.try_text(&text),
            Msg::Machine(tag) => {
                self.flow.select_machine(&tag);
            }
            Msg::Name(text) => {
                self.flow.set_administrator(&text);
            }
            Msg::Password(secret) => {
                self.flow.set_password(secret);
            }
            Msg::Confirmation(secret) => {
                self.flow.set_confirmation(secret);
            }
            Msg::Continue => {
                let step = self.flow.advance();
                self.carry_out(step, &sender);
            }
            Msg::Retry => {
                let step = self.flow.retry();
                self.carry_out(step, &sender);
            }
            Msg::Visible(page) => {
                // The view popped to `page` (Back, Escape): the flow steps
                // back to it, and refuses while the helper is asked (the view
                // is pushed again then).
                while self.flow.page() != page && self.flow.back().is_some() {}
            }
            Msg::Jump(page) => self.flow.show(page),
            Msg::Stats => self.flow.report_stats(),
            Msg::Ping(flag) => flag.store(true, Ordering::SeqCst),
        }
        self.print_markers();
        self.observe();
    }

    fn update_cmd(&mut self, message: Cmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            Cmd::Probed { generation, result } => {
                let step = self.flow.probe_finished(generation, result);
                self.carry_out(step, &sender);
            }
            Cmd::Planned { generation, result } => {
                let step = self.flow.plan_finished(generation, result);
                self.carry_out(step, &sender);
            }
            Cmd::Switched {
                generation,
                layout,
                result,
            } => {
                if let Err(why) = &result {
                    eprintln!("ilex-installer: switching the session's layout to {layout}: {why}");
                }
                let step = self.flow.keyboard_switched(generation, result);
                self.carry_out(step, &sender);
            }
        }
        self.print_markers();
        self.observe();
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: ComponentSender<Self>) {
        widgets.sync(&self.flow, &sender);
    }
}

// --- widgets ------------------------------------------------------------------

pub struct Widgets {
    nav: adw::NavigationView,
    welcome: Welcome,
    machine: Machine,
    account: Account,
    ready: Ready,
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

struct Account {
    page: adw::NavigationPage,
    banner: adw::Banner,
    name: adw::EntryRow,
    password: adw::PasswordEntryRow,
    confirmation: adw::PasswordEntryRow,
    name_message: gtk::Label,
    password_message: gtk::Label,
    button: gtk::Button,
}

struct Ready {
    page: adw::NavigationPage,
    stack: gtk::Stack,
    problem: adw::StatusPage,
    retry: gtk::Button,
    rows: Vec<(&'static str, adw::ActionRow)>,
}

fn sentence(message: &Message) -> String {
    if message.detail.is_empty() {
        message.title.clone()
    } else {
        format!("{}. {}", message.title, message.detail)
    }
}

/// A form page's body: a heading and a paragraph, then the controls, centred
/// and scrollable.
fn form(heading: &str, text: &str, controls: &[&gtk::Widget]) -> gtk::ScrolledWindow {
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

fn page(
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

fn continue_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("suggested-action");
    button.set_margin_top(6);
    button.set_margin_bottom(6);
    button
}

fn banner(button: Option<&str>) -> adw::Banner {
    let banner = adw::Banner::new("");
    banner.set_button_label(button);
    banner
}

fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    if !title.is_empty() {
        group.set_title(title);
    }
    if !description.is_empty() {
        group.set_description(Some(description));
    }
    group
}

fn message_label() -> gtk::Label {
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

fn property_row(title: &str) -> adw::ActionRow {
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
        root.set_content(Some(&nav));

        // --- welcome ---
        let language = combo(
            "Language",
            &LANGUAGES.iter().map(|l| l.label()).collect::<Vec<_>>(),
        );
        let keyboard = combo(
            "Keyboard layout",
            &flow
                .layouts()
                .all()
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>(),
        );
        let test = adw::EntryRow::builder()
            .title("Type here to try the layout")
            .build();
        let welcome_banner = banner(None);
        let welcome_button = continue_button("Continue");
        let language_group = group(
            "",
            "The language of the installed computer. The installer itself is in English.",
        );
        language_group.add(&language);
        let keyboard_group = group(
            "",
            "The layout is used here at once, and for the disk passphrase when the computer starts.",
        );
        keyboard_group.add(&keyboard);
        keyboard_group.add(&test);
        let keyboard_message = message_label();
        keyboard_message.set_hexpand(true);
        let keyboard_retry = gtk::Button::with_label("Try again");
        keyboard_retry.add_css_class("flat");
        keyboard_retry.set_valign(gtk::Align::Center);
        keyboard_retry.set_visible(false);
        let keyboard_line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        keyboard_line.append(&keyboard_message);
        keyboard_line.append(&keyboard_retry);
        keyboard_line.set_visible(false);
        let welcome_form = form(
            "Welcome to ilex",
            "This installs ilex on this computer. Nothing is changed until you confirm it on the last page.",
            &[
                language_group.upcast_ref(),
                keyboard_group.upcast_ref(),
                keyboard_line.upcast_ref(),
            ],
        );
        let welcome_page = page(
            "Welcome",
            PageId::Welcome.id(),
            Some(&welcome_banner),
            &welcome_form,
            Some(&welcome_button),
        );

        // --- machine ---
        let machine_banner = banner(None);
        let computer = property_row("This computer");
        let machine_group = group("Configuration", "");
        let notice = message_label();
        let machine_button = continue_button("Continue");
        let loading = adw::StatusPage::builder()
            .title("Looking at this computer")
            .description("The installer is reading what this computer is and which disks it has.")
            .child(&adw::Spinner::new())
            .build();
        let problem = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .build();
        let retry = gtk::Button::with_label("Try again");
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
            "This computer",
            "Pick the configuration to install. Computers ilex knows get their own; any other x86-64 computer uses the generic one.",
            &[machine_body.upcast_ref()],
        );
        stack.add_named(&machine_form, Some("choices"));
        let machine_page = page(
            "This computer",
            PageId::Machine.id(),
            Some(&machine_banner),
            &stack,
            Some(&machine_button),
        );
        let on_retry = sender.input_sender().clone();
        retry.connect_clicked(move |_| on_retry.emit(Msg::Probe));

        // --- account ---
        let name = adw::EntryRow::builder().title("User name").build();
        name.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
        let password = adw::PasswordEntryRow::builder()
            .title("Password")
            .name("password")
            .build();
        let confirmation = adw::PasswordEntryRow::builder()
            .title("Confirm the password")
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
        let account_button = continue_button("Continue");
        let account_form = form(
            "Administrator",
            "This account maintains the computer and recovers it when something goes wrong: it can use sudo. You create your own daily account when the computer starts for the first time.",
            &[
                name_group.upcast_ref(),
                name_message.upcast_ref(),
                password_group.upcast_ref(),
                password_message.upcast_ref(),
            ],
        );
        let account_banner = banner(None);
        let account_page = page(
            "Administrator",
            PageId::Account.id(),
            Some(&account_banner),
            &account_form,
            Some(&account_button),
        );

        // --- ready ---
        let ready_problem = adw::StatusPage::builder().build();
        let ready_retry = gtk::Button::with_label("Try again");
        ready_retry.add_css_class("pill");
        ready_retry.set_halign(gtk::Align::Center);
        ready_problem.set_child(Some(&ready_retry));
        let ready_running = adw::StatusPage::builder()
            .title(messages::CHECKING_TITLE)
            .description(messages::CHECKING_DETAIL)
            .child(&adw::Spinner::new())
            .build();
        let summary = group("Your choices", "");
        let image = group("The image", "");
        let mut rows = Vec::new();
        for (key, title, target) in [
            ("edition", "Edition", &summary),
            ("configuration", "Configuration", &summary),
            ("language", "Language", &summary),
            ("keyboard", "Keyboard layout", &summary),
            ("administrator", "Administrator", &summary),
            ("version", "Version", &image),
            ("download", "Download", &image),
            ("digest", "Digest", &image),
        ] {
            let row = property_row(title);
            target.add(&row);
            rows.push((key, row));
        }
        let end = gtk::Label::builder()
            .label("The choices are checked and nothing has been written to any disk. Choosing the disk, reviewing and installing come with the next version of the installer.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let checked = form(
            "Ready to continue",
            "The installer's helper checked these choices and the image they install.",
            &[summary.upcast_ref(), image.upcast_ref(), end.upcast_ref()],
        );
        let ready_stack = gtk::Stack::new();
        ready_stack.add_named(&ready_running, Some("running"));
        ready_stack.add_named(&ready_problem, Some("problem"));
        ready_stack.add_named(&checked, Some("checked"));
        let ready_page = page("Ready", PageId::Ready.id(), None, &ready_stack, None);

        for page in [&welcome_page, &machine_page, &account_page, &ready_page] {
            nav.add(page);
        }
        nav.set_pop_on_escape(true);

        // --- input ---
        let input = sender.input_sender().clone();
        language.connect_selected_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Language(row.selected()))
        });
        keyboard.connect_selected_notify({
            let input = input.clone();
            move |row| input.emit(Msg::Layout(row.selected()))
        });
        // The layout test field is read only when the flow says its text may be
        // printed (a test): people type their intended passwords there.
        if flow.echo_try() {
            test.connect_changed({
                let input = input.clone();
                move |row| input.emit(Msg::TryText(secret_of(row)))
            });
        }
        keyboard_retry.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::RetryLayout)
        });
        name.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Name(row.text().to_string()))
        });
        password.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Password(secret_of(row)))
        });
        confirmation.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Confirmation(secret_of(row)))
        });
        for banner in [&welcome_banner, &machine_banner, &account_banner] {
            banner.connect_button_clicked({
                let input = input.clone();
                move |_| input.emit(Msg::Probe)
            });
        }
        for button in [&welcome_button, &machine_button, &account_button] {
            button.connect_clicked({
                let input = input.clone();
                move |_| input.emit(Msg::Continue)
            });
        }
        ready_retry.connect_clicked({
            let input = input.clone();
            move |_| input.emit(Msg::Retry)
        });
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
                    input.emit(Msg::Visible(page));
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
        ] {
            row.connect_entry_activated({
                let input = input.clone();
                move |_| input.emit(Msg::Continue)
            });
        }
        // The first control of each page has the focus when the page is shown.
        for (page, focus) in [
            (&welcome_page, language.clone().upcast::<gtk::Widget>()),
            (&machine_page, machine_button.clone().upcast()),
            (&account_page, name.clone().upcast()),
        ] {
            page.connect_shown(move |_| {
                focus.grab_focus();
            });
        }

        // `add` shows the first page of an empty view before the handler above
        // exists: GTK's own first focus (the first row) stays.

        Self {
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
            account: Account {
                page: account_page,
                banner: account_banner,
                name,
                password,
                confirmation,
                name_message,
                password_message,
                button: account_button,
            },
            ready: Ready {
                page: ready_page,
                stack: ready_stack,
                problem: ready_problem,
                retry: ready_retry,
                rows,
            },
        }
    }

    /// Make every widget say what the flow says.
    fn sync(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        // Which page: the navigation view follows the flow (a page pushed by
        // Continue), and the flow follows the view when the person goes back.
        let wanted = flow.page();
        let shown = self
            .nav
            .visible_page()
            .and_then(|p| p.tag())
            .and_then(|t| PageId::from_id(&t));
        if shown != Some(wanted) {
            let index = |p: PageId| PageId::ORDER.iter().position(|q| *q == p);
            if shown.and_then(index) < index(wanted) {
                self.nav.push_by_tag(wanted.id());
            } else {
                self.nav.pop_to_tag(wanted.id());
            }
        }
        self.sync_welcome(flow);
        self.sync_machine(flow, sender);
        self.sync_account(flow);
        self.sync_ready(flow);
        // No way back while the helper is asked.
        let free = !flow.is_checking();
        self.machine.page.set_can_pop(free);
        self.account.page.set_can_pop(free);
        self.ready.page.set_can_pop(free);
    }

    fn sync_welcome(&mut self, flow: &Flow) {
        let w = &self.welcome;
        sync_banner(&w.banner, flow);
        if let Some(index) = language::index_of(flow.language())
            && w.language.selected() != index as u32
        {
            w.language.set_selected(index as u32);
        }
        if let Some(index) = flow.layouts().index_of(flow.keyboard())
            && w.keyboard.selected() != index as u32
        {
            w.keyboard.set_selected(index as u32);
        }
        match flow.keyboard_status() {
            KeyboardStatus::Applied => w.keyboard_line.set_visible(false),
            KeyboardStatus::Switching => {
                w.keyboard_message
                    .set_css_classes(&["caption", "dim-label"]);
                w.keyboard_message.set_label(messages::SWITCHING_LAYOUT);
                w.keyboard_message.set_visible(true);
                w.keyboard_retry.set_visible(false);
                w.keyboard_line.set_visible(true);
            }
            KeyboardStatus::Failed(message) => {
                w.keyboard_message.set_css_classes(&["caption", "error"]);
                w.keyboard_message.set_label(&sentence(&message));
                w.keyboard_message.set_visible(true);
                w.keyboard_retry.set_visible(true);
                w.keyboard_line.set_visible(true);
            }
        }
        w.button.set_label(flow.continue_label());
        w.button
            .set_sensitive(flow.page() != PageId::Welcome || flow.can_continue());
    }

    fn sync_machine(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        let m = &mut self.machine;
        m.button.set_label(flow.continue_label());
        m.button
            .set_sensitive(flow.page() != PageId::Machine || flow.can_continue());
        match (flow.probe_state(), flow.machine_view()) {
            (ProbeState::Pending, _) => m.stack.set_visible_child_name("loading"),
            (ProbeState::Failed(message), _) => {
                m.problem.set_title(&message.title);
                m.problem.set_description(Some(&message.detail));
                m.banner.set_revealed(false);
                m.stack.set_visible_child_name("problem");
            }
            (ProbeState::Ready(_), Some(view)) => {
                m.stack.set_visible_child_name("choices");
                m.computer.set_subtitle(&view.computer);
                rebuild_choices(m, view, sender);
                let selected = flow.machine_tag().unwrap_or_default();
                for (tag, check, _) in &m.rows {
                    if check.is_active() != (tag == selected) {
                        check.set_active(tag == selected);
                    }
                }
                // A problem that stops the installation (a BIOS boot found
                // after the person left the first page) comes before the
                // notice about how the configuration was chosen.
                let notice = view.notice();
                if flow.blocking_problem().is_some() {
                    sync_banner(&m.banner, flow);
                    m.notice.set_visible(true);
                    m.notice.set_label(&sentence(&notice.message));
                } else {
                    let loud = notice.severity != Severity::Info;
                    m.banner.set_title(&sentence(&notice.message));
                    m.banner.set_button_label(None);
                    m.banner.set_revealed(loud);
                    m.notice.set_visible(!loud);
                    m.notice.set_label(&sentence(&notice.message));
                }
            }
            (ProbeState::Ready(_), None) => {}
        }
    }

    fn sync_account(&mut self, flow: &Flow) {
        let a = &self.account;
        sync_banner(&a.banner, flow);
        // The choices cannot change while the helper checks them. A field that
        // has the focus gives it up first (GTK warns when a focused text
        // becomes insensitive), and the name has it again when the page is
        // editable once more.
        let editable = !flow.is_checking();
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
        if a.name.text() != flow.administrator() {
            a.name.set_text(flow.administrator());
        }
        let checks = flow.account_checks();
        show_message(
            &a.name_message,
            checks.name.message(),
            &[a.name.upcast_ref()],
        );
        let rows: [&gtk::Widget; 2] = [a.password.upcast_ref(), a.confirmation.upcast_ref()];
        // Only a refusal is an error; a hint is dimmed text, and the rows go
        // back to valid (the error style and the accessible state) as soon as
        // there is none.
        match checks
            .password
            .message()
            .or_else(|| checks.confirmation.message())
        {
            Some(refusal) => show_message(&a.password_message, Some(refusal), &rows),
            None => {
                show_message(&a.password_message, None, &rows);
                if let Some(advice) = flow.password_advice() {
                    a.password_message
                        .set_css_classes(&["caption", "dim-label"]);
                    a.password_message.set_label(&sentence(&advice));
                    a.password_message.set_visible(true);
                }
            }
        }
        a.button.set_label(flow.continue_label());
        a.button
            .set_sensitive(flow.page() != PageId::Account || flow.can_continue());
    }

    fn sync_ready(&mut self, flow: &Flow) {
        let r = &self.ready;
        match flow.plan_check() {
            PlanCheck::NotRun | PlanCheck::Running => r.stack.set_visible_child_name("running"),
            PlanCheck::Checked(plan) => {
                self.fill_summary(flow, plan);
                self.ready.stack.set_visible_child_name("checked");
            }
            PlanCheck::NoDisk(_) | PlanCheck::Failed(_) => {
                if let Some(message) = flow.check_problem() {
                    let icon = if matches!(flow.plan_check(), PlanCheck::NoDisk(_)) {
                        "drive-harddisk-symbolic"
                    } else {
                        "dialog-error-symbolic"
                    };
                    r.problem.set_icon_name(Some(icon));
                    r.problem.set_title(&message.title);
                    r.problem.set_description(Some(&message.detail));
                    // Not when only a restart helps (a BIOS boot).
                    r.retry.set_visible(flow.can_retry());
                    r.stack.set_visible_child_name("problem");
                }
            }
        }
    }

    fn fill_summary(&self, flow: &Flow, plan: &Plan) {
        let set = |key: &str, text: &str| {
            if let Some((_, row)) = self.ready.rows.iter().find(|(k, _)| *k == key) {
                row.set_subtitle(text);
            }
        };
        set("edition", flow.edition_name());
        set(
            "configuration",
            flow.machine_choice()
                .map_or(plan.composition.tag.as_str(), |c| c.title.as_str()),
        );
        set(
            "language",
            &language::find(flow.language())
                .map_or_else(|| flow.language().to_owned(), |l| l.label()),
        );
        set(
            "keyboard",
            flow.keyboard_layout()
                .map_or(flow.keyboard(), |l| l.name.as_str()),
        );
        set("administrator", flow.administrator());
        set(
            "version",
            plan.image.version.as_deref().unwrap_or("not reported"),
        );
        set("download", &human_bytes(plan.image.download_bytes));
        set("digest", &plan.image.digest);
    }
}

/// Show a refusal beside its field, or hide the label, and mark the rows to
/// match: the error style, and for assistive technology the rows' invalid
/// state with the message as their error message. With no refusal every row
/// goes back to valid, state and relation included.
fn show_message(label: &gtk::Label, refusal: Option<&Message>, rows: &[&gtk::Widget]) {
    match refusal {
        Some(message) => {
            label.set_css_classes(&["caption", "error"]);
            label.set_label(&sentence(message));
            label.set_visible(true);
        }
        None => label.set_visible(false),
    }
    for row in rows {
        if refusal.is_some() {
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

/// A page's banner: the problem that stops the installation, with Try again
/// when asking the helper again can mend it, whichever page the person is on.
fn sync_banner(banner: &adw::Banner, flow: &Flow) {
    let problem = flow.blocking_problem();
    banner.set_revealed(problem.is_some());
    if let Some(problem) = &problem {
        banner.set_title(&sentence(problem));
        banner.set_button_label(flow.can_retry_probe().then_some("Try again"));
    }
}

/// The text of a field, copied once from the toolkit's own buffer straight
/// into a `Secret`: no `String` or `GString` of it is made, so the only
/// copies are the field's (secure) buffer and a value that is wiped when it is
/// dropped.
fn secret_of(row: &impl IsA<gtk::Editable>) -> Secret {
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
fn rebuild_choices(m: &mut Machine, view: &MachineView, sender: &ComponentSender<App>) {
    let tags: Vec<&str> = view.choices.iter().map(|c| c.tag.as_str()).collect();
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
    for choice in &view.choices {
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
        let tag = choice.tag.clone();
        let input = sender.input_sender().clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                input.emit(Msg::Machine(tag.clone()));
            }
        });
        m.group.add(&row);
        m.rows.push((choice.tag.clone(), check, row));
    }
}
