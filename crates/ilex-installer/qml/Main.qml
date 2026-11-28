// The KDE edition's installer window: a Kirigami application window, the only
// window of the live session (KWin shows nothing else). It draws what the flow
// says (`installer.view`, one JSON document from `ilex-installer-flow`) and
// sends the person's inputs back as events; it holds no page logic, wording or
// visibility rule. Written to KDE's HIG: Kirigami pages in a page row, form
// layouts, inline messages, a prompt dialog for the one destructive question,
// the system's Breeze style and colours, sizes in Kirigami units.
import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.ilex.installer

Kirigami.ApplicationWindow {
    id: root

    title: installer.displayName
    visible: true
    // The session's only window fills the screen. `--windowed` (development)
    // is an ordinary window; the renderer's is the size its pictures have.
    visibility: installer.fullscreen ? Window.FullScreen : Window.Windowed
    width: installer.rendering ? 1280 : Kirigami.Units.gridUnit * 50
    height: installer.rendering ? 800 : Kirigami.Units.gridUnit * 34

    // The only window of a live session: the compositor's close request must
    // not end the installer, which would leave tty1 with nothing on it. Only
    // the session unit stops it.
    onClosing: close => {
        close.accepted = !installer.fullscreen;
    }

    readonly property var view: JSON.parse(installer.view)
    readonly property var catalogue: JSON.parse(installer.catalogue)
    readonly property bool ready: view.page !== undefined

    // The flow's glyphs, as this toolkit's icons.
    readonly property var icons: ({
            "error": "dialog-error",
            "disk": "drive-harddisk",
            "network": "network-disconnect",
            "success": "dialog-ok-apply"
        })

    // The administrator's password, the disk's passphrase and the recovery key
    // typed back are the flow's, not the window's (it reads the fields from
    // Rust, `sendField`, and a page that comes back is filled from the flow,
    // `restoreField`): when the flow says it wiped them (`account.wiped`, and a
    // new flow) the pages' fields are cleared.
    property var wipeMark: null
    signal wipe

    // A line for the application's own stderr (`console.log` goes nowhere).
    function logLine(text) {
        installer.renderReport("log", text);
    }

    function send(name, argument) {
        installer.send(name, argument === undefined ? "" : argument);
    }

    // An input whose text is read from the field itself, in Rust.
    function sendField(name, field) {
        installer.sendField(name, field);
    }
    function restoreField(name, field) {
        installer.restoreField(name, field);
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
        Component.onCompleted: {
            begin();
        }
        onRenderDo: (action, first, second) => root.renderRequest(action, first, second)
    }

    // The memory and uptime report of the boot trials comes this long after
    // the page, once the session has settled.
    Timer {
        id: settled
        interval: 15000
        onTriggered: installer.reportStats()
    }

    // --- the pages: a stack the flow decides ---------------------------------

    property bool syncing: false

    function pageComponent(id) {
        return {
            "welcome": welcomePage,
            "machine": machinePage,
            "disk": diskPage,
            "encryption": encryptionPage,
            "account": accountPage,
            "profile": profilePage,
            "review": reviewPage,
            "installing": installingPage,
            "recovery": recoveryPage,
            "done": donePage,
            "problem": problemPage
        }[id];
    }

    function heldPages() {
        const held = [];
        for (let i = 0; i < pageStack.depth; ++i) {
            held.push(pageStack.get(i).pageId);
        }
        return held;
    }

    // Make the page row hold exactly the flow's stack: one more is a push, fewer
    // is a pop, anything else (the pages an installation ends on have nothing
    // under them) replaces it. A page the row does not hold is never popped to.
    function syncStack() {
        if (!ready || view.starting) {
            return;
        }
        const want = view.stack;
        const have = heldPages();
        if (want.join() === have.join()) {
            // The row itself can have moved off the flow's page (its own Back
            // shortcut, Alt+Left, lowers the current index even while the flow
            // refuses to go back): the flow's page is the one shown.
            putBack();
            return;
        }
        syncing = true;
        const prefix = (shorter, longer) => shorter.every((id, i) => longer[i] === id);
        if (want.length === have.length + 1 && prefix(have, want)) {
            pageStack.push(pageComponent(want[want.length - 1]));
        } else if (want.length < have.length && prefix(want, have)) {
            pageStack.pop(pageStack.get(want.length - 1));
        } else {
            pageStack.clear();
            for (const id of want) {
                pageStack.push(pageComponent(id));
            }
        }
        syncing = false;
        putBack();
    }

    // The row shows the last page of the flow's stack: the flow's page.
    function putBack() {
        const last = view.stack.length - 1;
        if (pageStack.currentIndex === last) {
            return;
        }
        const shown = pageStack.currentItem;
        logLine("the page row showed " + (shown ? shown.pageId : "nothing") + " while the flow is on " + view.page + ": put back");
        syncing = true;
        pageStack.currentIndex = last;
        syncing = false;
    }

    onViewChanged: {
        syncStack();
        if (ready) {
            const mark = view.epoch + "/" + view.account.wiped;
            if (wipeMark !== null && wipeMark !== mark) {
                wipe();
            }
            wipeMark = mark;
        }
    }

    pageStack.columnView.columnResizeMode: Kirigami.ColumnView.SingleColumn
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.ToolBar
    // No way back while the helper is asked, or once the erase was confirmed;
    // and none from the pages an installation ends on: the flow says.
    pageStack.globalToolBar.showNavigationButtons: ready && currentChrome && currentChrome.can_pop ? Kirigami.ApplicationHeaderStyle.ShowBackButton : Kirigami.ApplicationHeaderStyle.NoNavigationButtons
    pageStack.globalToolBar.canContainHandles: false

    readonly property var currentChrome: ready && view[view.page] ? view[view.page].chrome : null

    // The row moved (Back, Escape): the flow follows, and refuses while the
    // helper is asked (the page is pushed again then).
    Connections {
        target: root.pageStack
        function onCurrentItemChanged() {
            const item = root.pageStack.currentItem;
            if (root.syncing || !item || !root.ready || item.pageId === root.view.page) {
                return;
            }
            root.send("visible", item.pageId);
            Qt.callLater(root.syncStack);
            Qt.callLater(() => root.reportPage());
        }
    }

    Shortcut {
        sequences: [StandardKey.Cancel]
        enabled: root.currentChrome !== null && root.currentChrome.can_pop && root.pageStack.depth > 1 && !root.view.confirm && !root.view.log
        onActivated: root.pageStack.pop()
    }

    Component {
        id: welcomePage
        WelcomePage {
            view: root.view
            catalogue: root.catalogue
            host: root
        }
    }
    Component {
        id: machinePage
        MachinePage {
            view: root.view
            host: root
        }
    }
    Component {
        id: diskPage
        DiskPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: encryptionPage
        EncryptionPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: accountPage
        AccountPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: profilePage
        ProfilePage {
            view: root.view
            host: root
        }
    }
    Component {
        id: reviewPage
        ReviewPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: installingPage
        InstallingPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: recoveryPage
        RecoveryPage {
            view: root.view
            host: root
        }
    }
    Component {
        id: donePage
        DonePage {
            view: root.view
            host: root
        }
    }
    Component {
        id: problemPage
        ProblemPage {
            view: root.view
            host: root
        }
    }

    // Until the helper has said where an installation is, nothing of the pages.
    Rectangle {
        anchors.fill: parent
        z: 100
        visible: !root.ready || !!root.view.starting
        color: Kirigami.Theme.backgroundColor
        Kirigami.LoadingPlaceholder {
            anchors.centerIn: parent
            text: root.ready && root.view.starting ? root.view.starting : ""
        }
    }

    // --- the dialogs: open exactly while the flow says so ----------------------

    ConfirmDialog {
        id: confirm
        view: root.view
        host: root
    }
    LogDialog {
        id: logDialog
        view: root.view
        host: root
    }

    // --- the renderer ------------------------------------------------------------

    function reportPage() {
        if (installer.rendering) {
            const item = pageStack.currentItem;
            installer.renderReport("page", item ? item.pageId : "");
        }
    }
    Connections {
        target: root.pageStack
        enabled: installer.rendering
        function onCurrentItemChanged() {
            root.reportPage();
        }
        function onDepthChanged() {
            root.reportPage();
        }
    }

    // What only the window can do for the renderer.
    function renderRequest(action, first, second) {
        switch (action) {
        case "flush":
            installer.renderReport("flush", "");
            break;
        case "back":
            if (pageStack.depth > 1) {
                pageStack.pop();
            }
            break;
        case "type":
            // A person types into the field: its own edit reaches the flow.
            {
                const item = pageStack.currentItem;
                const field = item && item.field ? item.field(first) : null;
                if (field) {
                    field.text = second;
                    field.textEdited();
                }
                installer.renderReport("typed", field ? "ok" : "no " + first + " field on " + (item ? item.pageId : "no page"));
            }
            break;
        case "switch":
            // A person flips a check box: its own toggle reaches the flow.
            {
                const item = pageStack.currentItem;
                const box = item && item.field ? item.field(first) : null;
                if (box) {
                    box.checked = second === "on";
                    box.toggled();
                }
                installer.renderReport("typed", box ? "ok" : "no " + first + " switch on " + (item ? item.pageId : "no page"));
            }
            break;
        case "fields":
            {
                // What the page's fields hold while the page exists (the flow's
                // own copies are wiped by the flow).
                const page = pageStack.currentItem;
                const live = name => page && page.field && page.field(name) ? page.field(name).text.length : 0;
                installer.renderReport("fields", [live("password"), live("confirmation"), live("passphrase"), live("passphrase-confirmation")].join(","));
            }
            break;
        case "expand-log":
            pageStack.currentItem.expandLog();
            break;
        case "quit":
            Qt.quit();
            break;
        }
    }
}
