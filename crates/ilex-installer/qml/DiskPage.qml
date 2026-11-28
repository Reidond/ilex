// Where to install: the disks the helper found, those it cannot use and why,
// and the way of installing (erase the disk, or install beside the other
// systems where the disk allows it, with the reason where it does not). Every
// state, word and row is the flow's (`view.disk`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "disk"
    readonly property var v: view.disk
    readonly property var body: v.body

    title: v.chrome.title
    header: ColumnLayout {
        spacing: 0
        Banner {
            host: page.host
            banner: page.v.chrome.banner
        }
        Banner {
            host: page.host
            banner: page.v.notice
        }
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
            visible: page.body.kind === "empty"
            icon.name: page.body.kind === "empty" ? page.host.icons[page.body.glyph] : ""
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
                text: page.body.heading || ""
            }
            QQC2.Label {
                Layout.fillWidth: true
                wrapMode: Text.Wrap
                text: page.body.intro || ""
            }
            Kirigami.Heading {
                level: 2
                text: page.body.group || ""
            }
            QQC2.ButtonGroup {
                id: group
            }
            Repeater {
                model: page.body.kind === "choices" ? page.body.offered : []
                delegate: Kirigami.RadioSubtitleDelegate {
                    required property var modelData
                    Layout.fillWidth: true
                    QQC2.ButtonGroup.group: group
                    text: modelData.title
                    subtitle: modelData.subtitle
                    checked: modelData.id === page.body.selected
                    onClicked: page.host.send("disk", modelData.id)
                }
            }
            Kirigami.Heading {
                level: 2
                visible: !!page.away
                text: page.away ? page.away.title : ""
            }
            QQC2.Label {
                Layout.fillWidth: true
                visible: !!page.away
                wrapMode: Text.Wrap
                text: page.away ? page.away.intro : ""
            }
            Repeater {
                model: page.away ? page.away.rows : []
                delegate: Kirigami.SubtitleDelegate {
                    required property var modelData
                    Layout.fillWidth: true
                    enabled: false
                    text: modelData.title
                    subtitle: modelData.subtitle
                }
            }
            Kirigami.Heading {
                level: 2
                text: page.body.modes_title || ""
            }
            QQC2.ButtonGroup {
                id: modeGroup
            }
            Repeater {
                model: page.body.kind === "choices" ? page.body.modes : []
                delegate: Kirigami.RadioSubtitleDelegate {
                    required property var modelData
                    objectName: "mode-" + modelData.id
                    Layout.fillWidth: true
                    QQC2.ButtonGroup.group: modeGroup
                    enabled: modelData.available
                    checked: modelData.selected
                    text: modelData.title
                    subtitle: modelData.subtitle
                    onClicked: page.host.send("mode", modelData.id)
                }
            }
        }
    }

    readonly property var away: page.body.kind === "choices" ? page.body.away : null
}
