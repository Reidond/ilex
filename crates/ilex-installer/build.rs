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
            .qml_file("qml/Main.qml"),
    )
    .files(["src/kde/installer.rs", "src/kde/platform.rs"])
    .build();
}

#[cfg(not(feature = "kde"))]
fn main() {}
