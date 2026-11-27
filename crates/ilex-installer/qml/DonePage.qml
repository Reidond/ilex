// Done: the installation succeeded. What was installed, and Restart and Power
// off, as the flow says them (`view.done`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "done"
    readonly property var v: view.done

    title: v ? v.chrome.title : ""

    function takeFocus() {
        restart.forceActiveFocus();
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
        Kirigami.FormLayout {
            Layout.fillWidth: true
            Repeater {
                model: page.v ? page.v.rows : []
                delegate: QQC2.Label {
                    required property var modelData
                    Kirigami.FormData.label: modelData.title
                    Layout.fillWidth: true
                    wrapMode: Text.WrapAnywhere
                    text: modelData.value
                }
            }
        }
        RowLayout {
            Layout.alignment: Qt.AlignHCenter
            spacing: Kirigami.Units.largeSpacing
            PressButton {
                id: restart
                visible: page.v ? page.v.restart.visible : false
                enabled: page.v ? page.v.restart.enabled : false
                highlighted: true
                icon.name: "system-reboot"
                text: page.v ? page.v.restart.label : ""
                onClicked: page.host.send("restart")
            }
            PressButton {
                visible: page.v ? page.v.power_off.visible : false
                enabled: page.v ? page.v.power_off.enabled : false
                icon.name: "system-shutdown"
                text: page.v ? page.v.power_off.label : ""
                onClicked: page.host.send("power-off")
            }
        }
        Note {
            Layout.alignment: Qt.AlignHCenter
            horizontalAlignment: Text.AlignHCenter
            note: page.v ? page.v.note : null
        }
    }
}
