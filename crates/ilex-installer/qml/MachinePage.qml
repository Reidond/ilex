// This computer: what the helper found and the configuration to install. The
// states (reading, a problem, the choices), the words and the notice are the
// flow's (`view.machine`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "machine"
    readonly property var v: view.machine
    readonly property var body: v.body

    title: v.chrome.title
    header: Banner {
        host: page.host
        banner: page.v.chrome.banner
    }
    footer: ContinueBar {
        id: bar
        host: page.host
        next: page.v.chrome.next
    }

    // The button has the focus when the page is shown.
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(bar.takeFocus);
        }
    }
    Component.onCompleted: Qt.callLater(bar.takeFocus)

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.LoadingPlaceholder {
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 14
            visible: page.body.kind === "loading"
            text: page.body.title || ""
            explanation: page.body.detail || ""
        }

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 14
            visible: page.body.kind === "problem"
            icon.name: page.host.icons.error
            text: page.body.title || ""
            explanation: page.body.detail || ""
            helpfulAction: Kirigami.Action {
                text: page.body.retry || ""
                icon.name: "view-refresh"
                onTriggered: page.host.send("probe")
            }
        }

        ColumnLayout {
            visible: page.body.kind === "choices"
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
                QQC2.Label {
                    Kirigami.FormData.label: page.body.computer_label || ""
                    text: page.body.computer || ""
                    wrapMode: Text.Wrap
                    Layout.fillWidth: true
                }
            }
            Note {
                note: page.body.notice ? {
                    "text": page.body.notice,
                    "error": false
                } : null
            }
            Kirigami.Heading {
                level: 2
                text: page.body.group || ""
            }
            QQC2.ButtonGroup {
                id: group
            }
            Repeater {
                model: page.body.kind === "choices" ? page.body.choices : []
                delegate: Kirigami.RadioSubtitleDelegate {
                    required property var modelData
                    Layout.fillWidth: true
                    QQC2.ButtonGroup.group: group
                    text: modelData.title
                    subtitle: modelData.subtitle
                    checked: modelData.id === page.body.selected
                    onClicked: page.host.send("machine", modelData.id)
                }
            }
        }
    }
}
