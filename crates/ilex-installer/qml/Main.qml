// The KDE edition's installer window: a Kirigami application window, the only
// window of the live session (KWin shows nothing else). Written to KDE's HIG:
// one page with a title, a placeholder message for what has no content yet, the
// system's Breeze style and colours, sizes in Kirigami units.
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.ilex.installer

Kirigami.ApplicationWindow {
    id: root

    title: installer.displayName
    visible: true
    // The session's only window fills the screen. `--windowed` (development)
    // is an ordinary window.
    visibility: installer.fullscreen ? Window.FullScreen : Window.Windowed
    width: Kirigami.Units.gridUnit * 50
    height: Kirigami.Units.gridUnit * 34

    // The only window of a live session: the compositor's close request must
    // not end the installer, which would leave tty1 with nothing on it. Only
    // the session unit stops it.
    onClosing: close => {
        close.accepted = !installer.fullscreen;
    }

    // What the first frame says is on screen, once.
    property bool shown: false
    onFrameSwapped: {
        if (shown) {
            return;
        }
        shown = true;
        installer.windowShown(rendererName(), Screen.devicePixelRatio, root.width, root.height, Screen.width, Screen.height);
        settled.start();
    }

    function rendererName(): string {
        switch (GraphicsInfo.api) {
        case GraphicsInfo.Software:
            return "software";
        case GraphicsInfo.OpenGL:
            return "opengl";
        default:
            return "other-" + GraphicsInfo.api;
        }
    }

    Installer {
        id: installer
    }

    // The memory and uptime report of the boot trials comes this long after
    // the page, once the session has settled.
    Timer {
        id: settled
        interval: 15000
        onTriggered: installer.reportStats()
    }

    pageStack.initialPage: Kirigami.Page {
        title: "Welcome"

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4

            icon.name: "system-software-install"
            text: installer.welcome
            explanation: "The installation pages are not part of this image yet."
        }
    }
}
