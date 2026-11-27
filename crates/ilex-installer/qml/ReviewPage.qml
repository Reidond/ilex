// Review: what will be installed and the one destructive button. Every group,
// row, sentence and the button's state is the flow's (`view.review`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "review"
    readonly property var v: view.review
    readonly property var body: v.body

    title: v.chrome.title

    // The destructive button has the focus when it appears (a page that was
    // already shown, after Try again, has no other moment to).
    readonly property bool buttonShown: v.button.visible
    function takeFocus() {
        if (buttonShown) {
            erase.forceActiveFocus();
        }
    }
    onButtonShownChanged: {
        if (buttonShown && isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    Component.onCompleted: Qt.callLater(takeFocus)

    footer: QQC2.ToolBar {
        visible: page.v.button.visible
        position: QQC2.ToolBar.Footer
        contentItem: RowLayout {
            Item {
                Layout.fillWidth: true
            }
            PressButton {
                id: erase
                icon.name: "edit-delete"
                text: page.v.button.label
                enabled: page.v.button.enabled
                // The destructive answer, as Breeze draws one.
                Kirigami.Theme.colorSet: Kirigami.Theme.Button
                palette.button: Kirigami.Theme.negativeTextColor
                palette.buttonText: Kirigami.Theme.highlightedTextColor
                palette.brightText: Kirigami.Theme.highlightedTextColor
                onClicked: page.host.send("install")
            }
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.LoadingPlaceholder {
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 14
            visible: page.body.kind === "waiting"
            text: page.body.title || ""
            explanation: page.body.detail || ""
        }

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 14
            visible: page.body.kind === "problem"
            icon.name: page.body.kind === "problem" ? page.host.icons[page.body.glyph] : ""
            text: page.body.title || ""
            explanation: page.body.detail || ""
            helpfulAction: Kirigami.Action {
                visible: page.body.kind === "problem" && page.body.retry.visible
                enabled: page.body.kind === "problem" && page.body.retry.enabled
                text: page.body.kind === "problem" ? page.body.retry.label : ""
                icon.name: "view-refresh"
                onTriggered: page.host.send("retry")
            }
        }

        ColumnLayout {
            visible: page.body.kind === "checked"
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
            Repeater {
                model: page.body.kind === "checked" ? page.body.groups : []
                delegate: ColumnLayout {
                    id: group
                    required property var modelData
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    Kirigami.Heading {
                        Layout.fillWidth: true
                        level: 2
                        text: group.modelData.title
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        visible: !!group.modelData.intro
                        wrapMode: Text.Wrap
                        font: Kirigami.Theme.smallFont
                        color: Kirigami.Theme.disabledTextColor
                        text: group.modelData.intro
                    }
                    Kirigami.FormLayout {
                        Layout.fillWidth: true
                        Repeater {
                            model: group.modelData.rows
                            delegate: QQC2.Label {
                                required property var modelData
                                Kirigami.FormData.label: modelData.title
                                Layout.fillWidth: true
                                wrapMode: Text.WrapAnywhere
                                text: modelData.value
                            }
                        }
                    }
                }
            }
            Kirigami.Heading {
                Layout.fillWidth: true
                level: 3
                wrapMode: Text.Wrap
                color: Kirigami.Theme.negativeTextColor
                text: page.body.erase || ""
            }
            QQC2.Label {
                Layout.fillWidth: true
                wrapMode: Text.Wrap
                color: Kirigami.Theme.disabledTextColor
                text: page.body.end || ""
            }
        }
    }
}
