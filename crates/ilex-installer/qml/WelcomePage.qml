// Welcome: the language of the installed computer and the keyboard layout, with
// a field to try the layout in. Every word and every state is the flow's
// (`view.welcome`); the two lists are the catalogue's.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var catalogue
    required property var host
    readonly property string pageId: "welcome"
    readonly property var v: view.welcome

    title: v.chrome.title
    header: Banner {
        host: page.host
        banner: page.v.chrome.banner
    }
    footer: ContinueBar {
        host: page.host
        next: page.v.chrome.next
    }

    function field(name) {
        return name === "try" ? tryField : null;
    }

    // The first control has the focus when the page is shown.
    function takeFocus() {
        languageBox.forceActiveFocus();
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    Component.onCompleted: Qt.callLater(takeFocus)

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Heading {
            Layout.fillWidth: true
            level: 1
            wrapMode: Text.Wrap
            text: page.v.heading
        }
        QQC2.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: page.v.intro
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true

            ColumnLayout {
                Kirigami.FormData.label: page.v.language.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                spacing: Kirigami.Units.smallSpacing
                QQC2.ComboBox {
                    id: languageBox
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    model: page.catalogue.languages
                    currentIndex: page.v.language.selected === null ? -1 : page.v.language.selected
                    onActivated: index => page.host.send("language", index)
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    wrapMode: Text.Wrap
                    font: Kirigami.Theme.smallFont
                    color: Kirigami.Theme.disabledTextColor
                    text: page.v.language.help
                }
            }

            ColumnLayout {
                Kirigami.FormData.label: page.v.keyboard.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                spacing: Kirigami.Units.smallSpacing
                // Several hundred layouts: a button with the current one that
                // opens a search dialog (a long combo box cannot be searched).
                PressButton {
                    id: layoutButton
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    icon.name: "input-keyboard"
                    text: page.v.keyboard.selected === null ? "" : page.catalogue.layouts[page.v.keyboard.selected]
                    onClicked: layoutDialog.open()
                }
                Kirigami.SearchDialog {
                    id: layoutDialog
                    parent: QQC2.Overlay.overlay
                    anchors.centerIn: parent
                    readonly property var matches: {
                        const wanted = text.toLowerCase();
                        return page.catalogue.layouts.map((name, index) => ({
                                    "name": name,
                                    "index": index
                                })).filter(entry => entry.name.toLowerCase().includes(wanted));
                    }
                    model: matches
                    searchFieldPlaceholderText: page.v.keyboard.label
                    onAccepted: {
                        if (currentItem) {
                            currentItem.clicked();
                        }
                    }
                    onClosed: {
                        text = "";
                        layoutButton.forceActiveFocus();
                    }
                    delegate: QQC2.ItemDelegate {
                        required property var modelData
                        width: ListView.view ? ListView.view.width : implicitWidth
                        text: modelData.name
                        onClicked: {
                            page.host.send("layout", modelData.index);
                            layoutDialog.close();
                        }
                    }
                }
                QQC2.TextField {
                    id: tryField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    placeholderText: page.v.try_label
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    // The flow reads this field only when it may print what
                    // is typed (`echo_try`): people type their passwords here.
                    onTextEdited: {
                        if (page.v.echo_try) {
                            page.host.sendField("try-text", tryField);
                        }
                    }
                    onAccepted: page.host.send("continue")
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    wrapMode: Text.Wrap
                    font: Kirigami.Theme.smallFont
                    color: Kirigami.Theme.disabledTextColor
                    text: page.v.keyboard.help
                }
                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    visible: page.v.keyboard_line !== null
                    type: page.v.keyboard_line && page.v.keyboard_line.failed ? Kirigami.MessageType.Error : Kirigami.MessageType.Information
                    text: page.v.keyboard_line ? page.v.keyboard_line.text : ""
                    actions: [
                        Kirigami.Action {
                            visible: page.v.keyboard_line !== null && !!page.v.keyboard_line.retry
                            text: page.v.keyboard_line && page.v.keyboard_line.retry ? page.v.keyboard_line.retry : ""
                            onTriggered: page.host.send("retry-layout")
                        }
                    ]
                }
            }
        }
    }
}
