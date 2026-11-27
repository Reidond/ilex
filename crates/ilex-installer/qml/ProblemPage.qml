// Problem: the installation failed, or the helper is not answering. What
// happened, the last lines of the log and what can be done are the flow's
// (`view.problem`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "problem"
    readonly property var v: view.problem

    title: v ? v.chrome.title : ""

    // Try again has the focus when there is one, else Restart.
    function takeFocus() {
        (retry.visible && retry.enabled ? retry : restart).forceActiveFocus();
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    Component.onCompleted: Qt.callLater(takeFocus)

    ColumnLayout {
        visible: !!page.v
        spacing: Kirigami.Units.largeSpacing

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.gridUnit * 2
            icon.name: page.v ? page.host.icons[page.v.glyph] : ""
            text: page.v ? page.v.heading : ""
            explanation: page.v ? page.v.detail : ""
        }
        ColumnLayout {
            Layout.fillWidth: true
            visible: page.v ? page.v.log.length > 0 : false
            Kirigami.Heading {
                level: 4
                text: page.v ? page.v.log_caption : ""
            }
            QQC2.ScrollView {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.gridUnit * 10
                QQC2.TextArea {
                    readOnly: true
                    wrapMode: TextEdit.WrapAnywhere
                    font.family: "monospace"
                    text: page.v ? page.v.log.join("\n") : ""
                }
            }
        }
        RowLayout {
            Layout.alignment: Qt.AlignHCenter
            spacing: Kirigami.Units.largeSpacing
            PressButton {
                id: retry
                visible: page.v ? page.v.retry.visible : false
                enabled: page.v ? page.v.retry.enabled : false
                highlighted: true
                icon.name: "view-refresh"
                text: page.v ? page.v.retry.label : ""
                onClicked: page.host.send("retry-install")
            }
            PressButton {
                visible: page.v ? page.v.save_log.visible : false
                enabled: page.v ? page.v.save_log.enabled : false
                icon.name: "document-save"
                text: page.v ? page.v.save_log.label : ""
                onClicked: page.host.send("choose-log")
            }
            PressButton {
                id: restart
                visible: page.v ? page.v.restart.visible : false
                enabled: page.v ? page.v.restart.enabled : false
                icon.name: "system-reboot"
                text: page.v ? page.v.restart.label : ""
                onClicked: page.host.send("restart")
            }
        }
        Note {
            Layout.alignment: Qt.AlignHCenter
            horizontalAlignment: Text.AlignHCenter
            note: page.v ? page.v.note : null
        }
    }
}
