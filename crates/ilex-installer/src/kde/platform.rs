//! The two Qt calls the Qt Quick Controls bindings of cxx-qt-lib do not carry:
//! the icon theme Kirigami's icons are looked up in. Without a KDE platform
//! theme (Plasma's integration is not in this image) Qt's icon theme is
//! `hicolor`, which has none of Breeze's names.

#[cxx_qt::bridge]
mod ffi {
    unsafe extern "C++" {
        include!(<QtGui/QIcon>);
        /// The icon class: only its static theme setter is used here.
        type QIcon;

        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        /// Sets the name of the current icon theme.
        #[Self = "QIcon"]
        #[rust_name = "set_theme_name"]
        fn setThemeName(name: &QString);
    }
}

/// Make `name` (an installed icon theme, `breeze`) the application's.
pub fn set_icon_theme(name: &str) {
    ffi::QIcon::set_theme_name(&cxx_qt_lib::QString::from(name));
}
