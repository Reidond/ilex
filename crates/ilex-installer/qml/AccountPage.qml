// The administrator: a name and a password, with the refusals beside the
// fields. What each field says and when it is editable is the flow's
// (`view.account`). The password fields are read from Rust (`sendField`: the
// text is not passed through a JavaScript string), and a page that comes back
// has them filled from what the flow holds (`restoreField`) until it says it is
// wiped.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "account"
    readonly property var v: view.account
    readonly property string nameValue: v.name_value

    title: v.chrome.title
    header: Banner {
        host: page.host
        banner: page.v.chrome.banner
    }
    footer: ContinueBar {
        host: page.host
        next: page.v.chrome.next
    }

    function field(name) {
        return {
            "name": nameField,
            "password": passwordField,
            "confirmation": confirmationField
        }[name];
    }

    function takeFocus() {
        if (v.editable) {
            nameField.forceActiveFocus();
        }
    }
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    // The name has it again when the page is editable once more.
    readonly property bool editable: v.editable
    onEditableChanged: {
        if (editable && isCurrentPage) {
            Qt.callLater(takeFocus);
        }
    }
    // The flow's name, when it is not what the field holds (the flow decided
    // it, or the field was just made).
    onNameValueChanged: {
        if (nameField.text !== nameValue) {
            nameField.text = nameValue;
        }
    }
    Component.onCompleted: {
        Qt.callLater(takeFocus);
        nameField.text = nameValue;
        host.restoreField("password", passwordField);
        host.restoreField("confirmation", confirmationField);
    }
    Connections {
        target: page.host
        function onWipe() {
            passwordField.clear();
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
            enabled: page.v.editable

            ColumnLayout {
                Kirigami.FormData.label: page.v.name.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                QQC2.TextField {
                    id: nameField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhNoPredictiveText | Qt.ImhLowercaseOnly | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.send("name", text)
                    onAccepted: page.host.send("continue")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.v.name_note
                }
            }

            ColumnLayout {
                Kirigami.FormData.label: page.v.password.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                Kirigami.PasswordField {
                    id: passwordField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.sendField("password", passwordField)
                    onAccepted: page.host.send("continue")
                }
            }

            ColumnLayout {
                Kirigami.FormData.label: page.v.confirmation.label
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                Kirigami.PasswordField {
                    id: confirmationField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.sendField("confirmation", confirmationField)
                    onAccepted: page.host.send("continue")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.v.password_note
                }
            }
        }
    }
}
