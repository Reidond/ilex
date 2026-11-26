//! The application: an `AdwNavigationView` with the welcome, machine, account
//! and ready pages, each drawn from the flow's state. The flow decides which
//! page is next and whether Continue works; this file turns its state into
//! widgets and the person's input into flow calls. Nothing here talks to the
//! helper except through the flow's `Backend`, on a worker thread.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::flow::{Flow, PageId, PlanCheck, ProbeState, Step, human_bytes};
use ilex_installer_flow::keyboard::{Layouts, SessionKeyboard};
use ilex_installer_flow::language::{self, LANGUAGES};
use ilex_installer_flow::machine::{MachineView, Severity};
use ilex_installer_flow::messages::ClientError;
use ilex_installer_flow::Message;
use ilex_installer_model::plan::Plan;
use ilex_installer_model::probe::Probe;
use relm4::adw::{self, prelude::*};
use relm4::gtk::{self, glib};
use relm4::prelude::*;

use crate::serial;

/// Memory is sampled this long after the page is shown, once the session has
/// settled, for the boot report.
const STATS_DELAY: Duration = Duration::from_secs(15);

pub struct Init {
    pub backend: Arc<dyn Backend>,
    pub keyboard: Arc<dyn SessionKeyboard + Send + Sync>,
    pub layouts: Layouts,
    pub fullscreen: bool,
    /// The edition identifier, for the serial markers.
    pub edition: String,
    /// Print marker lines (the real session); the renderer turns them off.
    pub markers: bool,
    /// Report memory fifteen seconds after the first page.
    pub stats: bool,
}

pub struct App {
    flow: Flow,
    backend: Arc<dyn Backend>,
    keyboard: Arc<dyn SessionKeyboard + Send + Sync>,
    edition: String,
    markers: bool,
    stats: bool,
    mapped: bool,
    /// The page whose marker was printed last.
    announced: Option<PageId>,
    /// What the layout test field held last, to print only changes.
    tried: String,
}

#[derive(Debug)]
pub enum Msg {
    /// Ask the helper about this computer (again).
    Probe,
    Language(u32),
    Layout(u32),
    TryText(String),
    Machine(String),
    Name(String),
    Password(String),
    Confirmation(String),
    Continue,
    /// The check of the choices once more (the ready page's "Try again").
    PlanAgain,
    /// The navigation view now shows this page (the person went back).
    Visible(PageId),
    /// The window is on screen.
    Mapped,
    /// Show a page directly (the renderer only).
    Jump(PageId),
}

#[derive(Debug)]
pub enum Cmd {
    Probed(Result<Probe, ClientError>),
    Planned(Result<Plan, ClientError>),
    Switched { layout: String, result: Result<(), String> },
}

impl App {
    fn marker(&self, line: &str) {
        if self.markers {
            serial::marker(line);
        }
    }

    fn start_probe(&mut self, sender: &ComponentSender<Self>) {
        self.flow.probe_restarted();
        let backend = Arc::clone(&self.backend);
        sender.spawn_oneshot_command(move || Cmd::Probed(backend.probe()));
    }

    /// Print the page's marker (and the composition line on the machine page)
    /// when the flow's page changed.
    fn announce(&mut self, root: &adw::ApplicationWindow) {
        let page = self.flow.page();
        if self.announced == Some(page) {
            return;
        }
        let first = self.announced.is_none();
        self.announced = Some(page);
        let mut line = format!(
            "ILEX-INSTALLER page={} edition={} {}",
            page.id(),
            self.edition,
            self.flow.marker_fields()
        );
        if first {
            let renderer = root
                .native()
                .and_then(|native| native.renderer())
                .map_or_else(|| "none".to_owned(), |r| r.type_().name().to_owned());
            line.push_str(&format!(
                " renderer={renderer} scale={} uptime={}",
                root.scale_factor(),
                uptime()
            ));
        }
        self.marker(&line);
        if page == PageId::Machine
            && let Some(composition) = self.flow.composition_line()
        {
            self.marker(&composition);
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

    fn init(
        init: Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let flow = Flow::new(init.layouts);
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
            edition: init.edition,
            markers: init.markers,
            stats: init.stats,
            mapped: false,
            announced: None,
            tried: String::new(),
        };
        model.start_probe(&sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Msg, sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            Msg::Probe => self.start_probe(&sender),
            Msg::Mapped => {
                if !self.mapped {
                    self.mapped = true;
                    self.announce(root);
                    if self.stats {
                        glib::timeout_add_local_once(STATS_DELAY, || {
                            serial::marker(&format!(
                                "ILEX-INSTALLER-STATS mem_total_kib={} mem_available_kib={} shmem_kib={} uptime={}",
                                meminfo("MemTotal"),
                                meminfo("MemAvailable"),
                                meminfo("Shmem"),
                                uptime()
                            ));
                        });
                    }
                }
            }
            Msg::Language(index) => {
                if let Some(language) = LANGUAGES.get(index as usize)
                    && self.flow.language() != language.locale
                    && self.flow.set_language(language.locale)
                {
                    self.marker(&format!(
                        "ILEX-INSTALLER event=language language={}",
                        language.locale
                    ));
                }
            }
            Msg::Layout(index) => {
                let id = self.flow.layouts().all().get(index as usize).map(|l| l.id.clone());
                if let Some(id) = id
                    && self.flow.keyboard() != id
                    && self.flow.set_keyboard(&id)
                    && let Some(layout) = self.flow.layouts().get(&id).cloned()
                {
                    // The session types in the new layout from now on; the
                    // call blocks on D-Bus, so it runs on a worker.
                    let keyboard = Arc::clone(&self.keyboard);
                    sender.spawn_oneshot_command(move || Cmd::Switched {
                        layout: layout.id.clone(),
                        result: keyboard.switch(&layout).map_err(|e| format!("{e:#}")),
                    });
                }
            }
            Msg::TryText(text) => {
                if text != self.tried {
                    self.tried.clone_from(&text);
                    // The test field holds nothing private: it exists to be
                    // typed in so that the layout can be seen to work.
                    let shown: String = text
                        .chars()
                        .take(32)
                        .map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c })
                        .collect();
                    self.marker(&format!(
                        "ILEX-INSTALLER event=try layout={} text={shown}",
                        self.flow.keyboard()
                    ));
                }
            }
            Msg::Machine(tag) => {
                if self.flow.machine_tag() != Some(tag.as_str()) && self.flow.select_machine(&tag)
                    && let Some(line) = self.flow.composition_line()
                {
                    self.marker(&line);
                }
            }
            Msg::Name(text) => self.flow.set_administrator(&text),
            Msg::Password(text) => self.flow.set_password(&text),
            Msg::Confirmation(text) => self.flow.set_confirmation(&text),
            Msg::Continue | Msg::PlanAgain => {
                let step = if matches!(message, Msg::PlanAgain) {
                    self.flow.retry_plan()
                } else {
                    self.flow.advance()
                };
                if let Step::RunPlan(request) = step {
                    let backend = Arc::clone(&self.backend);
                    sender.spawn_oneshot_command(move || Cmd::Planned(backend.plan(&request)));
                }
            }
            Msg::Visible(page) => {
                if self.flow.page() != page {
                    self.flow.show(page);
                }
            }
            Msg::Jump(page) => self.flow.show(page),
        }
        self.announce(root);
    }

    fn update_cmd(&mut self, message: Cmd, _sender: ComponentSender<Self>, root: &Self::Root) {
        match message {
            Cmd::Probed(result) => {
                let outcome = match &result {
                    Ok(probe) => format!("ok disks={}", probe.disks.len()),
                    Err(error) => format!("failed {}", error.message().title.replace(' ', "_")),
                };
                self.flow.set_probe(result);
                self.marker(&format!("ILEX-INSTALLER event=probe result={outcome}"));
                // The machine page may be showing already: its marker and the
                // composition line follow the answer.
                if self.flow.page() == PageId::Machine {
                    self.announced = None;
                }
            }
            Cmd::Planned(result) => {
                let outcome = match &result {
                    Ok(_) => "ok".to_owned(),
                    Err(ClientError::Refused(error)) => format!("refused code={:?}", error.code),
                    Err(_) => "failed".to_owned(),
                };
                self.flow.plan_finished(result);
                self.marker(&format!("ILEX-INSTALLER event=plan result={outcome}"));
            }
            Cmd::Switched { layout, result } => {
                self.marker(&format!(
                    "ILEX-INSTALLER event=layout layout={layout} applied={}",
                    result.is_ok()
                ));
                if let Err(why) = result {
                    eprintln!("ilex-installer: switching the session's layout to {layout}: {why}");
                }
            }
        }
        self.announce(root);
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
    page: adw::NavigationPage,
    banner: adw::Banner,
    language: adw::ComboRow,
    keyboard: adw::ComboRow,
    test: adw::EntryRow,
    button: gtk::Button,
}

struct Machine {
    page: adw::NavigationPage,
    banner: adw::Banner,
    stack: gtk::Stack,
    loading: adw::StatusPage,
    problem: adw::StatusPage,
    computer: adw::ActionRow,
    group: adw::PreferencesGroup,
    notice: gtk::Label,
    rows: Vec<(String, gtk::CheckButton, adw::ActionRow)>,
    button: gtk::Button,
}

struct Account {
    page: adw::NavigationPage,
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
            "The layout is used here at once, and for the disk passphrase and your password when the computer starts. Super+Space switches between the layouts of a session that has several.",
        );
        keyboard_group.add(&keyboard);
        keyboard_group.add(&test);
        let welcome_form = form(
            "Welcome to ilex",
            "This installs ilex on this computer. Nothing is changed until you confirm it on the last page.",
            &[language_group.upcast_ref(), keyboard_group.upcast_ref()],
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
        name.set_activates_default(true);
        name.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::LOWERCASE);
        let password = adw::PasswordEntryRow::builder().title("Password").build();
        password.set_activates_default(true);
        let confirmation = adw::PasswordEntryRow::builder()
            .title("Confirm the password")
            .build();
        confirmation.set_activates_default(true);
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
        let account_page = page(
            "Administrator",
            PageId::Account.id(),
            None,
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
            .title("Checking your choices")
            .description("The installer is asking its helper to check them against this computer and the image registry.")
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
        test.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::TryText(row.text().to_string()))
        });
        name.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Name(row.text().to_string()))
        });
        password.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Password(row.text().to_string()))
        });
        confirmation.connect_changed({
            let input = input.clone();
            move |row| input.emit(Msg::Confirmation(row.text().to_string()))
        });
        for banner in [&welcome_banner, &machine_banner] {
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
            move |_| input.emit(Msg::PlanAgain)
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
        // Enter continues where it can, and the first control has the focus.
        for (page, button, focus) in [
            (&welcome_page, &welcome_button, language.clone().upcast::<gtk::Widget>()),
            (&machine_page, &machine_button, machine_button.clone().upcast()),
            (&account_page, &account_button, name.clone().upcast()),
        ] {
            let window = root.clone();
            let button = button.clone();
            page.connect_showing(move |_| window.set_default_widget(Some(&button)));
            page.connect_shown(move |_| {
                focus.grab_focus();
            });
        }

        // `add` shows the first page of an empty view.

        Self {
            nav,
            welcome: Welcome {
                page: welcome_page,
                banner: welcome_banner,
                language,
                keyboard,
                test,
                button: welcome_button,
            },
            machine: Machine {
                page: machine_page,
                banner: machine_banner,
                stack,
                loading,
                problem,
                computer,
                group: machine_group,
                notice,
                rows: Vec::new(),
                button: machine_button,
            },
            account: Account {
                page: account_page,
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
        let busy = matches!(flow.plan_check(), PlanCheck::Running);
        self.account.page.set_can_pop(!busy);
        self.ready.page.set_can_pop(!busy);
    }

    fn sync_welcome(&mut self, flow: &Flow) {
        let w = &self.welcome;
        let problem = flow.blocking_problem();
        w.banner.set_revealed(problem.is_some());
        if let Some(problem) = &problem {
            w.banner.set_title(&sentence(problem));
            let retry = matches!(flow.probe_state(), ProbeState::Failed(_));
            w.banner.set_button_label(retry.then_some("Try again"));
        }
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
        w.button.set_sensitive(flow.can_continue() || flow.page() != PageId::Welcome);
        let _ = (&w.page, &w.test);
    }

    fn sync_machine(&mut self, flow: &Flow, sender: &ComponentSender<App>) {
        let m = &mut self.machine;
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
                let notice = view.notice();
                let loud = notice.severity != Severity::Info;
                m.banner.set_title(&sentence(&notice.message));
                m.banner.set_revealed(loud);
                m.notice.set_visible(!loud);
                m.notice.set_label(&sentence(&notice.message));
            }
            (ProbeState::Ready(_), None) => {}
        }
        let _ = (&m.page, &m.loading, &m.group);
    }

    fn sync_account(&mut self, flow: &Flow) {
        let a = &self.account;
        if a.name.text() != flow.administrator() {
            a.name.set_text(flow.administrator());
        }
        if a.password.text() != flow.password().expose() {
            a.password.set_text(flow.password().expose());
        }
        let checks = flow.account_checks();
        show_message(&a.name_message, &[checks.name.message().cloned()], &[&a.name], None);
        let advice = flow.password_advice();
        let wording = [
            checks.password.message().cloned(),
            checks.confirmation.message().cloned(),
        ];
        let rows: [&adw::PasswordEntryRow; 2] = [&a.password, &a.confirmation];
        // Only a refusal is an error; a hint is dimmed text.
        if wording.iter().all(Option::is_none) {
            a.password_message.set_css_classes(&["caption", "dim-label"]);
            match &advice {
                Some(advice) => {
                    a.password_message.set_label(&sentence(advice));
                    a.password_message.set_visible(true);
                }
                None => a.password_message.set_visible(false),
            }
            for row in rows {
                row.remove_css_class("error");
            }
        } else {
            let refused = wording.iter().flatten().next().cloned();
            show_message(
                &a.password_message,
                &[refused],
                &[&a.password, &a.confirmation],
                None,
            );
        }
        let busy = matches!(flow.plan_check(), PlanCheck::Running);
        a.button.set_label(if busy { "Checking…" } else { "Continue" });
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
            PlanCheck::NoDisk(reasons) => {
                r.problem.set_icon_name(Some("drive-harddisk-symbolic"));
                r.problem.set_title("No disk can be installed to");
                r.problem.set_description(Some(&format!(
                    "{}\n\nConnect a disk of at least {} that nothing is using, then try again.",
                    reasons.join("\n"),
                    human_bytes(ilex_installer_model::plan::MIN_DISK_BYTES)
                )));
                r.retry.set_visible(true);
                r.stack.set_visible_child_name("problem");
            }
            PlanCheck::Failed(message) => {
                r.problem.set_icon_name(Some("dialog-error-symbolic"));
                r.problem.set_title(&message.title);
                r.problem.set_description(Some(&message.detail));
                r.retry.set_visible(true);
                r.stack.set_visible_child_name("problem");
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
            flow.machine_choice().map_or(plan.composition.tag.as_str(), |c| c.title.as_str()),
        );
        set(
            "language",
            &language::find(flow.language()).map_or_else(|| flow.language().to_owned(), |l| l.label()),
        );
        set(
            "keyboard",
            flow.keyboard_layout().map_or(flow.keyboard(), |l| l.name.as_str()),
        );
        set("administrator", flow.administrator());
        set("version", plan.image.version.as_deref().unwrap_or("not reported"));
        set("download", &human_bytes(plan.image.download_bytes));
        set("digest", &plan.image.digest);
    }
}

/// Show the first refusal beside its field, and mark the rows.
fn show_message(
    label: &gtk::Label,
    messages: &[Option<Message>],
    rows: &[&impl IsA<gtk::Widget>],
    _hint: Option<&Message>,
) {
    match messages.iter().flatten().next() {
        Some(message) => {
            label.set_css_classes(&["caption", "error"]);
            label.set_label(&sentence(message));
            label.set_visible(true);
            for row in rows {
                row.add_css_class("error");
            }
        }
        None => {
            label.set_visible(false);
            for row in rows {
                row.remove_css_class("error");
            }
        }
    }
}

/// One radio row per configuration, rebuilt only when the list changes.
fn rebuild_choices(m: &mut Machine, view: &MachineView, sender: &ComponentSender<App>) {
    let tags: Vec<&str> = view.choices.iter().map(|c| c.tag.as_str()).collect();
    if m.rows.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>() == tags {
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

fn meminfo(key: &str) -> String {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Seconds since boot, from the kernel.
fn uptime() -> String {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}
