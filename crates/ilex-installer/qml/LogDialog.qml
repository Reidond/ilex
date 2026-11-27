// Save the log: which USB stick the installer's log goes to (a prompt dialog,
// whose layout takes the content). Every state and
// word is the flow's (`view.log`); the dialog is open exactly while the flow
// has it, and cannot be closed while the log is being written.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.PromptDialog {
    id: dialog

    required property var view
    required property var host
    readonly property var l: view.log ? view.log : null
    readonly property var body: l ? l.body : ({})

    title: l ? l.title : ""
    dialogType: Kirigami.PromptDialog.None
    standardButtons: Kirigami.Dialog.NoButton
    closePolicy: l && l.can_close ? QQC2.Popup.CloseOnEscape : QQC2.Popup.NoAutoClose
    preferredWidth: Kirigami.Units.gridUnit * 32
    maximumWidth: Kirigami.Units.gridUnit * 40

    onLChanged: {
        if (l) {
            if (!visible) {
                open();
            }
        } else if (visible) {
            close();
        }
    }
    onClosed: host.send("close-log")
    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.LoadingPlaceholder {
            Layout.fillWidth: true
            visible: dialog.body.kind === "looking" || dialog.body.kind === "saving"
            text: dialog.body.title || ""
            explanation: dialog.body.detail || ""
        }

        ColumnLayout {
            Layout.fillWidth: true
            visible: dialog.body.kind === "choosing"
            spacing: Kirigami.Units.largeSpacing
            Kirigami.Heading {
                level: 3
                text: dialog.body.group || ""
            }
            QQC2.Label {
                Layout.fillWidth: true
                wrapMode: Text.Wrap
                text: dialog.body.intro || ""
            }
            Repeater {
                model: dialog.body.kind === "choosing" ? dialog.body.sticks : []
                delegate: RowLayout {
                    id: stick
                    required property var modelData
                    Layout.fillWidth: true
                    enabled: !modelData.problem
                    Kirigami.TitleSubtitle {
                        Layout.fillWidth: true
                        title: stick.modelData.title
                        subtitle: stick.modelData.subtitle
                    }
                    PressButton {
                        visible: !stick.modelData.problem
                        highlighted: true
                        text: stick.modelData.save
                        onClicked: dialog.host.send("save-log", stick.modelData.id)
                    }
                }
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: dialog.body.kind === "choosing" && dialog.body.sticks.length === 0
                wrapMode: Text.Wrap
                color: Kirigami.Theme.disabledTextColor
                text: dialog.body.empty || ""
            }
            PressButton {
                Layout.alignment: Qt.AlignRight
                text: dialog.body.look || ""
                onClicked: dialog.host.send("choose-log")
            }
        }

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            visible: dialog.body.kind === "saved" || dialog.body.kind === "failed"
            icon.name: dialog.body.glyph ? dialog.host.icons[dialog.body.glyph] : ""
            text: dialog.body.title || ""
            explanation: dialog.body.detail || ""
        }
        RowLayout {
            Layout.alignment: Qt.AlignHCenter
            visible: dialog.body.kind === "saved" || dialog.body.kind === "failed"
            PressButton {
                visible: dialog.body.kind === "failed"
                text: dialog.body.retry || ""
                onClicked: dialog.host.send("choose-log")
            }
            PressButton {
                highlighted: dialog.body.kind === "saved"
                text: dialog.body.close || ""
                onClicked: dialog.close()
            }
        }
    }
}
