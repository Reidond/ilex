// The page's Continue button (or the one that takes its place), bottom right,
// as the flow says it.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

QQC2.ToolBar {
    id: bar

    required property var host
    property var next: null
    // What the button sends and how it looks: Continue by default; the
    // recovery page's Confirm sends its own.
    property string event: "continue"
    property string iconName: "go-next"

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
            icon.name: bar.iconName
            text: bar.next ? bar.next.label : ""
            enabled: bar.next ? bar.next.enabled : false
            highlighted: true
            onClicked: bar.host.send(bar.event)
        }
    }
}
