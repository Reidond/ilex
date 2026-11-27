// The page's Continue button, bottom right, as the flow says it.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

QQC2.ToolBar {
    id: bar

    required property var host
    property var next: null

    visible: next !== null && next !== undefined
    position: QQC2.ToolBar.Footer

    function takeFocus() {
        button.forceActiveFocus();
    }

    contentItem: RowLayout {
        Item {
            Layout.fillWidth: true
        }
        PressButton {
            id: button
            icon.name: "go-next"
            text: bar.next ? bar.next.label : ""
            enabled: bar.next ? bar.next.enabled : false
            highlighted: true
            onClicked: bar.host.send("continue")
        }
    }
}
