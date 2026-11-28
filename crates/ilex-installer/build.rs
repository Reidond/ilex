//! The Qt side of the KDE edition's interface (the `kde` feature): cxx-qt
//! generates the C++ for the QObjects declared in `src/kde/` and Qt's own tools
//! compile the QML module (`qml/`) into the binary's resources, loaded as
//! `qrc:/qt/qml/org/ilex/installer/qml/<file>`. Without the feature there is
//! nothing to build here and no Qt is needed.

#[cfg(feature = "kde")]
fn main() {
    use cxx_qt_build::{CxxQtBuilder, QmlModule};

    CxxQtBuilder::new_qml_module(
        QmlModule::new("org.ilex.installer")
            .version(1, 0)
            .qml_files([
                "qml/Main.qml",
                "qml/Banner.qml",
                "qml/ContinueBar.qml",
                "qml/Note.qml",
                "qml/PressButton.qml",
                "qml/WelcomePage.qml",
                "qml/MachinePage.qml",
                "qml/DiskPage.qml",
                "qml/EncryptionPage.qml",
                "qml/AccountPage.qml",
                "qml/ReviewPage.qml",
                "qml/InstallingPage.qml",
                "qml/RecoveryPage.qml",
                "qml/DonePage.qml",
                "qml/ProblemPage.qml",
                "qml/ConfirmDialog.qml",
                "qml/LogDialog.qml",
            ]),
    )
    .qt_module("Quick")
    .include_dir("cpp")
    .cpp_file("cpp/ilex_grab.cpp")
    .files(["src/kde/installer.rs", "src/kde/platform.rs"])
    .build();
    // The renderer's `grabWindow` shim (cpp/ilex_grab.cpp) calls into Qt Quick,
    // which the cxx-qt-lib bindings do not link by themselves: after the
    // generated code that uses it, or `--as-needed` drops it.
    println!("cargo:rustc-link-lib=Qt6Quick");
}

#[cfg(not(feature = "kde"))]
fn main() {}
