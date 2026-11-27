// Installing: the stages, the overall progress and the log, as the flow says
// them (`view.installing`). The log's disclosure is the page's own.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "installing"
    readonly property var v: view.installing

    title: v ? v.chrome.title : ""
    header: Banner {
        host: page.host
        banner: page.v ? page.v.chrome.banner : null
    }

    property bool logOpen: false
    function expandLog() {
        logOpen = true;
    }

    ColumnLayout {
        visible: !!page.v
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Heading {
            Layout.fillWidth: true
            level: 1
            wrapMode: Text.Wrap
            text: page.v ? page.v.heading : ""
        }
        QQC2.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: page.v ? page.v.intro : ""
        }
        QQC2.ProgressBar {
            Layout.fillWidth: true
            from: 0
            to: 1
            value: page.v ? page.v.overall : 0
        }
        QQC2.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            font: Kirigami.Theme.smallFont
            color: Kirigami.Theme.disabledTextColor
            text: page.v ? page.v.message : ""
        }
        Repeater {
            model: page.v ? page.v.rows : []
            delegate: RowLayout {
                id: row
                required property var modelData
                Layout.fillWidth: true
                spacing: Kirigami.Units.largeSpacing
                Item {
                    implicitWidth: Kirigami.Units.iconSizes.smallMedium
                    implicitHeight: Kirigami.Units.iconSizes.smallMedium
                    QQC2.BusyIndicator {
                        anchors.fill: parent
                        running: visible
                        visible: row.modelData.status === "running"
                    }
                    Kirigami.Icon {
                        anchors.fill: parent
                        visible: row.modelData.status !== "running"
                        source: ({
                                "pending": "media-record",
                                "done": "dialog-ok-apply",
                                "failed": "dialog-error"
                            })[row.modelData.status] || "media-record"
                        opacity: row.modelData.status === "pending" ? 0.4 : 1
                    }
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: row.modelData.label
                }
                QQC2.ProgressBar {
                    visible: row.modelData.status === "running"
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 8
                    from: 0
                    to: 1
                    value: row.modelData.fraction
                }
            }
        }
        PressButton {
            id: disclosure
            checkable: true
            checked: page.logOpen
            icon.name: checked ? "arrow-down" : "arrow-right"
            text: page.v ? page.v.show_log : ""
            onToggled: page.logOpen = checked
        }
        QQC2.ScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 10
            visible: page.logOpen
            QQC2.TextArea {
                readOnly: true
                wrapMode: TextEdit.WrapAnywhere
                font.family: "monospace"
                text: page.v ? page.v.log.join("\n") : ""
            }
        }
    }
}
