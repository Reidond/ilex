// Encryption: whether the disk is encrypted, whether the computer's security
// chip unlocks it, and the passphrase typed twice, with the refusals beside the
// fields. What each control says and when it can be used is the flow's
// (`view.encryption`). The passphrase fields are read from Rust (`sendField`:
// the text is not passed through a JavaScript string), and a page that comes
// back has them filled from what the flow holds (`restoreField`) until it says
// it is wiped.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "encryption"
    readonly property var v: view.encryption

    title: v.chrome.title
    header: Banner {
        host: page.host
        banner: page.v.chrome.banner
    }
    footer: ContinueBar {
        host: page.host
        next: page.v.chrome.next
    }

    // What the renderer and the trials reach by name.
    function field(name) {
        return {
            "encrypt": encryptBox,
            "chip": chipBox,
            "passphrase": passphraseField,
            "passphrase-confirmation": confirmationField
        }[name];
    }

    // The page starts on the passphrase while encryption is on (what nearly
    // everyone does first), else on the checkbox.
    function takeFocus() {
        if (v.editable) {
            passphraseField.forceActiveFocus();
        } else if (v.encrypt.enabled) {
            encryptBox.forceActiveFocus();
        }
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    // The passphrase has it again when the fields can be typed in once more.
    readonly property bool editable: v.editable
    onEditableChanged: {
        if (editable && isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    Component.onCompleted: {
        Qt.callLater(takeFocus);
        host.restoreField("passphrase", passphraseField);
        host.restoreField("passphrase-confirmation", confirmationField);
    }
    Connections {
        target: page.host
        function onWipe() {
            passphraseField.clear();
            confirmationField.clear();
        }
    }

    ColumnLayout {
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

            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                QQC2.CheckBox {
                    id: encryptBox
                    text: page.v.encrypt.title
                    enabled: page.v.encrypt.enabled
                    onToggled: page.host.send("encrypt", checked ? "on" : "off")
                    // The box follows the flow (a restarted interface, the
                    // renderer); the flow ignores an echo of its own value.
                    Binding {
                        target: encryptBox
                        property: "checked"
                        value: page.v.encrypt.on
                        restoreMode: Binding.RestoreNone
                    }
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    note: ({
                            "text": page.v.encrypt.line,
                            "error": false
                        })
                }
            }

            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                QQC2.CheckBox {
                    id: chipBox
                    text: page.v.chip.title
                    enabled: page.v.chip.enabled
                    onToggled: page.host.send("chip", checked ? "on" : "off")
                    Binding {
                        target: chipBox
                        property: "checked"
                        value: page.v.chip.on
                        restoreMode: Binding.RestoreNone
                    }
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    note: ({
                            "text": page.v.chip.line,
                            "error": false
                        })
                }
            }

            ColumnLayout {
                visible: page.v.fields
                Kirigami.FormData.label: page.v.passphrase.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                Kirigami.PasswordField {
                    id: passphraseField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    enabled: page.v.editable
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.sendField("passphrase", passphraseField)
                    onAccepted: page.host.send("continue")
                }
            }

            ColumnLayout {
                visible: page.v.fields
                Kirigami.FormData.label: page.v.confirmation.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                Kirigami.PasswordField {
                    id: confirmationField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    enabled: page.v.editable
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.sendField("passphrase-confirmation", confirmationField)
                    onAccepted: page.host.send("continue")
                }
                // Under both fields: what layout the passphrase is typed in,
                // then a refusal or advice.
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: ({
                            "text": page.v.layout_line,
                            "error": false
                        })
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.v.note
                }
            }

            // While encryption is off: what that means.
            Kirigami.InlineMessage {
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                visible: page.v.off_line !== null
                type: Kirigami.MessageType.Warning
                text: page.v.off_line === null ? "" : page.v.off_line
            }
        }
    }
}
