//! The QObject the QML window talks to (`Installer` in the `org.ilex.installer`
//! module): what the first page shows, and the two calls that print the
//! marker lines of the session. The flow lives in it: the QML never decides
//! what is printed or when, it only says that the window is on screen.

use std::cell::RefCell;
use std::pin::Pin;

use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;
use ilex_installer_flow::flow::Flow;

use crate::serial;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        // What never changes while the window lives: read once by the QML.
        #[qproperty(QString, edition, cxx_name = "edition", READ, CONSTANT)]
        #[qproperty(QString, display_name, cxx_name = "displayName", READ, CONSTANT)]
        #[qproperty(QString, welcome, READ, CONSTANT)]
        #[qproperty(bool, fullscreen, READ, CONSTANT)]
        type Installer = super::InstallerRust;

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
    }
}

/// What `kde::run` gives the object QML is about to create.
pub struct Handover {
    pub edition: String,
    pub display_name: String,
    pub fullscreen: bool,
    pub flow: Flow,
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

pub struct InstallerRust {
    edition: QString,
    display_name: QString,
    welcome: QString,
    fullscreen: bool,
    flow: Option<Flow>,
}

impl Default for InstallerRust {
    fn default() -> Self {
        let handover = HANDOVER.with(|slot| slot.borrow_mut().take());
        match handover {
            Some(handover) => Self {
                edition: QString::from(handover.edition.as_str()),
                welcome: QString::from(format!("Welcome to {}", handover.display_name).as_str()),
                display_name: QString::from(handover.display_name.as_str()),
                fullscreen: handover.fullscreen,
                flow: Some(handover.flow),
            },
            // Created by something that did not go through `kde::run`: show
            // the page, print nothing.
            None => Self {
                edition: QString::default(),
                display_name: QString::default(),
                welcome: QString::from("Welcome"),
                fullscreen: false,
                flow: None,
            },
        }
    }
}

impl InstallerRust {
    /// Print what the flow queued, in its order: a line the flow did not queue
    /// is not printed.
    fn print_markers(&mut self) {
        if let Some(flow) = self.flow.as_mut() {
            for line in flow.drain_markers() {
                serial::marker(&line);
            }
        }
    }
}

impl qobject::Installer {
    pub fn window_shown(
        self: Pin<&mut Self>,
        renderer: &QString,
        scale: f64,
        window_width: i32,
        window_height: i32,
        screen_width: i32,
        screen_height: i32,
    ) {
        let rust = self.rust_mut().get_mut();
        if let Some(flow) = rust.flow.as_mut() {
            flow.window_shown(&format!(
                "renderer={renderer} scale={scale} window={window_width}x{window_height} screen={screen_width}x{screen_height}"
            ));
        }
        rust.print_markers();
    }

    pub fn report_stats(self: Pin<&mut Self>) {
        let rust = self.rust_mut().get_mut();
        if let Some(flow) = rust.flow.as_mut() {
            flow.report_stats();
        }
        rust.print_markers();
    }
}
