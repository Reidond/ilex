//! The QObject the QML window talks to (`Installer` in the `org.ilex.installer`
//! module). It holds the flow and nothing else: every input the QML sees is
//! sent to it as an event name and one text argument (`send`), every change it
//! makes comes back as one JSON document (`view`, `ilex_installer_flow::view`)
//! that the QML only draws. No page's logic, wording or visibility rule is in
//! the QML or in this file: the flow decides (`Flow::handle`, `Flow::view`,
//! `Services::job`) and this carries its steps out on worker threads, giving
//! each answer back on the Qt thread.

use std::cell::RefCell;
use std::pin::Pin;
use std::sync::Arc;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use ilex_installer_flow::events::{Event, Observed};
use ilex_installer_flow::flow::Flow;
use ilex_installer_flow::keyboard::Layouts;
use ilex_installer_flow::runner::{Answer, Services};
use ilex_installer_flow::simulate::Simulated;
use serde::Serialize;
use zeroize::Zeroize;

use super::render::Shared;
use crate::serial;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("ilex_grab.h");
        /// The `text` of a QML text field: read here, so the text does not
        /// pass through a JavaScript string for every keystroke.
        #[rust_name = "item_text"]
        unsafe fn ilexItemText(item: *mut QObject) -> QString;
        /// Put `text` in a QML text field (a page that comes back).
        #[rust_name = "set_item_text"]
        unsafe fn ilexSetItemText(item: *mut QObject, text: &QString);
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        // What never changes while the window lives: read once by the QML.
        #[qproperty(QString, edition, cxx_name = "edition", READ, CONSTANT)]
        #[qproperty(QString, display_name, cxx_name = "displayName", READ, CONSTANT)]
        #[qproperty(bool, fullscreen, READ, CONSTANT)]
        // The renderer (`--self-check --render`) drives the window: the QML
        // answers its requests (`renderDo`).
        #[qproperty(bool, rendering, READ, CONSTANT)]
        // The two lists a page chooses from, as JSON.
        #[qproperty(QString, catalogue, READ, CONSTANT)]
        // The whole window's state as one JSON document.
        #[qproperty(QString, view)]
        type Installer = super::InstallerRust;

        /// The window exists: ask the helper where an installation is.
        #[qinvokable]
        fn begin(self: Pin<&mut Installer>);

        /// One input: the event's name and its text argument
        /// (`ilex_installer_flow::events::Event::parse`).
        #[qinvokable]
        fn send(self: Pin<&mut Installer>, name: &QString, argument: &QString);

        /// One input from a text field: the event's name, and the field itself,
        /// whose text is read here rather than passed as a JavaScript string.
        /// The text is wiped once the event is parsed.
        ///
        /// # Safety
        /// `item` is a live QML text field (it is read on the Qt thread).
        #[qinvokable]
        #[cxx_name = "sendField"]
        unsafe fn send_field(self: Pin<&mut Installer>, name: &QString, item: *mut QObject);

        /// A page that comes back puts what the flow holds of a password, the
        /// disk's passphrase or the recovery key typed back in its field
        /// (`password`, `confirmation`, `passphrase`, `passphrase-confirmation`
        /// or `recovery-key`).
        ///
        /// # Safety
        /// `item` is a live QML text field (it is written on the Qt thread).
        #[qinvokable]
        #[cxx_name = "restoreField"]
        unsafe fn restore_field(self: Pin<&mut Installer>, name: &QString, item: *mut QObject);

        /// The window is on screen: the flow queues the first page's marker,
        /// with Qt's own words (the renderer, the scale, the sizes), and it is
        /// printed. Calling it again prints nothing.
        #[qinvokable]
        #[cxx_name = "windowShown"]
        fn window_shown(
            self: Pin<&mut Installer>,
            renderer: &QString,
            scale: f64,
            window_width: i32,
            window_height: i32,
            screen_width: i32,
            screen_height: i32,
        );

        /// The session has settled: the memory and uptime report.
        #[qinvokable]
        #[cxx_name = "reportStats"]
        fn report_stats(self: Pin<&mut Installer>);

        /// The renderer's window answers a request (`kind`, `value`).
        #[qinvokable]
        #[cxx_name = "renderReport"]
        fn render_report(self: Pin<&mut Installer>, kind: &QString, value: &QString);

        /// The renderer asks the window to do something only the window can
        /// (`type`, `switch`, `back`, `expand-log`, `fields`, `quit`).
        #[qsignal]
        #[cxx_name = "renderDo"]
        fn render_do(self: Pin<&mut Installer>, action: &QString, first: &QString, second: &QString);
    }

    impl cxx_qt::Threading for Installer {}
}

/// What `kde::run` gives the object QML is about to create.
pub struct Handover {
    pub edition: String,
    pub display_name: String,
    pub fullscreen: bool,
    pub flow: Flow,
    pub services: Services,
    /// Set when this is the renderer's window.
    pub render: Option<Arc<Shared>>,
    /// The keyboard layouts (for the renderer to build a flow per shot).
    pub layouts: Layouts,
}

thread_local! {
    /// QML creates `Installer` on the main thread, by default construction;
    /// this is how it receives what the launch decided.
    static HANDOVER: RefCell<Option<Handover>> = const { RefCell::new(None) };
}

/// Leave `handover` for the `Installer` the QML creates (on this thread).
pub fn hand_over(handover: Handover) {
    HANDOVER.with(|slot| *slot.borrow_mut() = Some(handover));
}

/// The document QML reads: the flow's view, and which fresh flow it is (the
/// renderer replaces the flow between shots; the fields are cleared then).
#[derive(Serialize)]
struct Document<'a> {
    epoch: u64,
    #[serde(flatten)]
    view: &'a ilex_installer_flow::view::View,
}

struct Running {
    flow: Flow,
    services: Services,
    epoch: u64,
}

pub struct InstallerRust {
    edition: QString,
    display_name: QString,
    fullscreen: bool,
    rendering: bool,
    catalogue: QString,
    view: QString,
    running: Option<Running>,
    render: Option<Arc<Shared>>,
    layouts: Option<Layouts>,
}

impl Default for InstallerRust {
    fn default() -> Self {
        let handover = HANDOVER.with(|slot| slot.borrow_mut().take());
        match handover {
            Some(handover) => {
                let catalogue = serde_json::to_string(&handover.flow.catalogue())
                    .unwrap_or_else(|_| "{}".to_owned());
                Self {
                    edition: QString::from(handover.edition.as_str()),
                    display_name: QString::from(handover.display_name.as_str()),
                    fullscreen: handover.fullscreen,
                    rendering: handover.render.is_some(),
                    catalogue: QString::from(catalogue.as_str()),
                    view: QString::from("{}"),
                    running: Some(Running {
                        flow: handover.flow,
                        services: handover.services,
                        epoch: 0,
                    }),
                    render: handover.render,
                    layouts: Some(handover.layouts),
                }
            }
            // Created by something that did not go through `kde::run`: show
            // nothing, print nothing.
            None => Self {
                edition: QString::default(),
                display_name: QString::default(),
                fullscreen: false,
                rendering: false,
                catalogue: QString::from("{}"),
                view: QString::from("{}"),
                running: None,
                render: None,
                layouts: None,
            },
        }
    }
}

impl InstallerRust {
    /// Print what the flow queued, in its order: a line the flow did not queue
    /// is not printed.
    fn print_markers(&mut self) {
        if let Some(running) = self.running.as_mut() {
            for line in running.flow.drain_markers() {
                serial::marker(&line);
            }
        }
    }
}

impl qobject::Installer {
    pub fn begin(mut self: Pin<&mut Self>) {
        if let Some(shared) = self.rust().render.clone() {
            shared.attach(self.qt_thread());
        }
        let step = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.running.as_mut().map(|running| running.flow.start())
        };
        if let Some(step) = step {
            self.as_mut().carry_out(step);
        }
        self.refresh();
    }

    pub fn send(self: Pin<&mut Self>, name: &QString, argument: &QString) {
        self.deliver(&String::from(name), String::from(argument));
    }

    /// # Safety
    /// `item` is a live QML text field.
    pub unsafe fn send_field(self: Pin<&mut Self>, name: &QString, item: *mut qobject::QObject) {
        // SAFETY: the caller's.
        let text = String::from(&unsafe { qobject::item_text(item) });
        self.deliver(&String::from(name), text);
    }

    /// # Safety
    /// `item` is a live QML text field.
    pub unsafe fn restore_field(self: Pin<&mut Self>, name: &QString, item: *mut qobject::QObject) {
        let text = self.rust().running.as_ref().and_then(|running| {
            let secret = match String::from(name).as_str() {
                "password" => running.flow.password(),
                "confirmation" => running.flow.confirmation(),
                "passphrase" => running.flow.passphrase(),
                "passphrase-confirmation" => running.flow.passphrase_confirmation(),
                "recovery-key" => running.flow.recovery_typed(),
                _ => return None,
            };
            (!secret.is_empty()).then(|| QString::from(secret.expose()))
        });
        if let Some(text) = text {
            // SAFETY: the caller's.
            unsafe { qobject::set_item_text(item, &text) };
        }
    }

    /// Parse an input and do what it means. `text` may be a password: the copy
    /// made on the way in is wiped after parsing (the event holds a `Secret`
    /// of its own).
    fn deliver(self: Pin<&mut Self>, name: &str, mut text: String) {
        let parsed = Event::parse(name, &text);
        text.zeroize();
        match parsed {
            Ok(event) => self.dispatch(event),
            Err(error) => eprintln!("ilex-installer: ignoring an input: {error}"),
        }
    }

    pub fn window_shown(
        mut self: Pin<&mut Self>,
        renderer: &QString,
        scale: f64,
        window_width: i32,
        window_height: i32,
        screen_width: i32,
        screen_height: i32,
    ) {
        let rust = self.as_mut().rust_mut().get_mut();
        if let Some(running) = rust.running.as_mut() {
            running.flow.window_shown(&format!(
                "renderer={renderer} scale={scale} window={window_width}x{window_height} screen={screen_width}x{screen_height}"
            ));
        }
        rust.print_markers();
        if let Some(shared) = &rust.render {
            shared.set_scale(scale);
        }
    }

    pub fn report_stats(self: Pin<&mut Self>) {
        self.dispatch(Event::Stats);
    }

    pub fn render_report(self: Pin<&mut Self>, kind: &QString, value: &QString) {
        // Qt's own log (`console.log`) goes nowhere in this application: a
        // window that has something to say says it here.
        if kind.to_string() == "log" {
            eprintln!("ilex-installer: qml: {value}");
            return;
        }
        if let Some(shared) = &self.rust().render {
            shared.report(&String::from(kind), &String::from(value));
        }
    }

    /// Do what `event` means and carry out the step it returns.
    pub fn dispatch(mut self: Pin<&mut Self>, event: Event) {
        let step = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.running.as_mut().map(|running| running.flow.handle(event))
        };
        if let Some(step) = step {
            self.as_mut().carry_out(step);
        }
        self.refresh();
    }

    /// A worker's answer, on the Qt thread.
    fn answered(mut self: Pin<&mut Self>, answer: Answer) {
        let step = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.running.as_mut().map(|running| running.flow.answer(answer))
        };
        if let Some(step) = step {
            self.as_mut().carry_out(step);
        }
        self.refresh();
    }

    /// Run the work `step` asks for on a worker thread; the answer comes back
    /// to [`Self::answered`] on the Qt thread, and the flow ignores it if a
    /// later request replaced it (it carries the request's generation).
    fn carry_out(self: Pin<&mut Self>, step: ilex_installer_flow::flow::Step) {
        let job = self
            .rust()
            .running
            .as_ref()
            .and_then(|running| running.services.job(step));
        let Some(job) = job else { return };
        let thread = self.qt_thread();
        std::thread::spawn(move || {
            let answer = job();
            // The window may be gone by now; then there is nothing to tell.
            let _ = thread.queue(move |installer| installer.answered(answer));
        });
    }

    /// Print what the flow queued, publish what it observes (for the renderer)
    /// and hand the QML its new view.
    fn refresh(mut self: Pin<&mut Self>) {
        let json = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.print_markers();
            let Some(running) = rust.running.as_ref() else {
                return;
            };
            if let Some(shared) = &rust.render {
                shared.observe(running.flow.observed());
            }
            serde_json::to_string(&Document {
                epoch: running.epoch,
                view: &running.flow.view(),
            })
            .unwrap_or_else(|_| "{}".to_owned())
        };
        let mut json = json;
        self.set_view(QString::from(json.as_str()));
        // The view can hold the recovery key; the QString copy is Qt's.
        json.zeroize();
    }

    /// The renderer's next shot: a new flow over the scenario's helper, as a
    /// freshly started installer would have.
    pub fn reset_for_shot(mut self: Pin<&mut Self>, scenario: &str, services: Services) {
        let step = {
            let rust = self.as_mut().rust_mut().get_mut();
            let Some(layouts) = rust.layouts.clone() else {
                return;
            };
            let epoch = rust.running.as_ref().map_or(0, |r| r.epoch) + 1;
            let mut services = services;
            match Simulated::load(scenario) {
                Ok(backend) => services.backend = Arc::new(backend),
                Err(error) => {
                    eprintln!("ilex-installer: scenario {scenario}: {error:#}");
                    return;
                }
            }
            if let Some(shared) = &rust.render {
                shared.observe(Observed::initial());
            }
            let mut flow = Flow::new(layouts);
            let step = flow.start();
            rust.running = Some(Running {
                flow,
                services,
                epoch,
            });
            step
        };
        self.as_mut().carry_out(step);
        self.refresh();
    }

    /// The renderer's picture: the window drawn to `path`, and the answer.
    pub fn grab_to(self: Pin<&mut Self>, path: &str) {
        let answer = match super::platform::grab(path) {
            Ok(()) => "ok".to_owned(),
            Err(why) => why,
        };
        if let Some(shared) = &self.rust().render {
            shared.report("grab", &answer);
        }
    }

    /// Ask the window to do one of the things only it can.
    pub fn ask_window(self: Pin<&mut Self>, action: &str, first: &str, second: &str) {
        self.render_do(
            &QString::from(action),
            &QString::from(first),
            &QString::from(second),
        );
    }
}
