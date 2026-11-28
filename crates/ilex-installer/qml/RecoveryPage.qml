// Recovery key: the key in eight numbered groups (while this window holds it),
// the field for the whole key typed from the paper, and, when the key cannot be
// shown or the volume refused it, the way out (continue without a recovery
// key). What it says, shows and allows is the flow's (`view.recovery`); the key
// is on screen only for as long as the flow holds it, and is not selectable.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "recovery"
    readonly property var v: view.recovery

    title: v ? v.chrome.title : ""
    footer: ContinueBar {
        host: page.host
        event: "confirm-recovery"
        iconName: "dialog-ok-apply"
        next: page.v ? ({
                "label": page.v.confirm.label,
                "enabled": page.v.confirm.enabled
            }) : null
    }

    function field(name) {
        return {
            "recovery-key": keyField
        }[name];
    }

    // The field has the focus when the page is shown.
    function takeFocus() {
        keyField.forceActiveFocus();
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    Component.onCompleted: {
        Qt.callLater(takeFocus);
        host.restoreField("recovery-key", keyField);
    }
    // The flow wiped the key it holds (confirmed, refused, given up, or the
    // installation ended): the field is cleared too.
    Connections {
        target: page.host
        function onWipe() {
            keyField.clear();
        }
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

        // The key, eight groups in two rows of four, numbered.
        Kirigami.AbstractCard {
            Layout.fillWidth: true
            visible: !!page.v && page.v.key !== null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.largeSpacing
                Kirigami.Heading {
                    Layout.fillWidth: true
                    level: 3
                    wrapMode: Text.Wrap
                    color: Kirigami.Theme.neutralTextColor
                    text: page.v && page.v.warning ? page.v.warning : ""
                }
                GridLayout {
                    Layout.alignment: Qt.AlignHCenter
                    columns: 4
                    rowSpacing: Kirigami.Units.largeSpacing
                    columnSpacing: Kirigami.Units.gridUnit * 2
                    Repeater {
                        model: page.v && page.v.key ? page.v.key : []
                        delegate: ColumnLayout {
                            id: cell
                            required property int index
                            required property string modelData
                            spacing: 0
                            QQC2.Label {
                                Layout.alignment: Qt.AlignHCenter
                                font: Kirigami.Theme.smallFont
                                color: Kirigami.Theme.disabledTextColor
                                text: cell.index + 1
                            }
                            QQC2.Label {
                                Layout.alignment: Qt.AlignHCenter
                                font.family: "monospace"
                                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.6
                                text: cell.modelData
                            }
                        }
                    }
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    font: Kirigami.Theme.smallFont
                    color: Kirigami.Theme.disabledTextColor
                    text: page.v && page.v.keep ? page.v.keep : ""
                }
            }
        }

        // Why there is no key to show: being fetched, shown before a restart,
        // or could not be fetched.
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: !!page.v && page.v.no_key !== null
            type: Kirigami.MessageType.Warning
            text: page.v && page.v.no_key ? page.v.no_key.text : ""
        }

        QQC2.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: page.v ? page.v.ask : ""
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true
            ColumnLayout {
                Kirigami.FormData.label: page.v ? page.v.field.label : ""
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                QQC2.TextField {
                    id: keyField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    font.family: "monospace"
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    // The field stays as it is while the helper compares (a
                    // disabled field would give up the focus, and a refusal
                    // would leave a person with none): the flow ignores what
                    // is typed meanwhile.
                    onTextEdited: page.host.sendField("recovery-key", keyField)
                    onAccepted: page.host.send("confirm-recovery")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    note: page.v ? ({
                            "text": page.v.hint,
                            "error": false
                        }) : null
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    note: page.v && page.v.check ? ({
                            "text": page.v.check.text,
                            "error": true
                        }) : null
                }
            }
        }

        // The helper's refusal of what was typed.
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: !!page.v && page.v.problem !== null
            type: Kirigami.MessageType.Error
            text: page.v && page.v.problem ? page.v.problem.text : ""
        }

        // The way out: what the disk is then, and the button.
        ColumnLayout {
            Layout.fillWidth: true
            visible: !!page.v && page.v.skip !== null
            spacing: Kirigami.Units.smallSpacing
            Kirigami.Heading {
                Layout.fillWidth: true
                level: 3
                wrapMode: Text.Wrap
                text: page.v && page.v.skip ? page.v.skip.title : ""
            }
            QQC2.Label {
                Layout.fillWidth: true
                wrapMode: Text.Wrap
                text: page.v && page.v.skip ? page.v.skip.detail : ""
            }
            Note {
                note: page.v && page.v.skip && page.v.skip.problem ? ({
                        "text": page.v.skip.problem.text,
                        "error": true
                    }) : null
            }
            PressButton {
                id: skipButton
                icon.name: "go-next-skip"
                text: page.v && page.v.skip ? page.v.skip.button.label : ""
                enabled: page.v && page.v.skip ? page.v.skip.button.enabled : false
                onClicked: page.host.send("skip-recovery")
            }
        }

        // The installation goes on behind the page.
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
            text: page.v ? page.v.progress_line : ""
        }
    }
}
