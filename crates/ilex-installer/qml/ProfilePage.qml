// The daily user: no profile, a public profile of the image, or a user name of
// the person's choice with an optional capacity. What is offered, what each
// row says, which are available and what the fields' refusals are is the
// flow's (`view.profile`).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "profile"
    readonly property var v: view.profile
    readonly property var custom: v.custom
    readonly property string nameValue: custom ? custom.name_value : ""
    readonly property string sizeValue: custom ? custom.size_value : ""

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

    function field(name) {
        return {
            "daily-name": nameField,
            "daily-size": sizeField
        }[name];
    }

    // The button has the focus when the page is shown.
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(bar.takeFocus);
        }
    }
    Component.onCompleted: {
        Qt.callLater(bar.takeFocus);
        nameField.text = nameValue;
        sizeField.text = sizeValue;
    }
    // The flow's text, when it is not what the field holds (the flow decided
    // it, or the field was just made).
    onNameValueChanged: {
        if (nameField.text !== nameValue) {
            nameField.text = nameValue;
        }
    }
    onSizeValueChanged: {
        if (sizeField.text !== sizeValue) {
            sizeField.text = sizeValue;
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
        Kirigami.Heading {
            level: 2
            text: page.v.group
        }
        QQC2.ButtonGroup {
            id: group
        }
        Repeater {
            model: page.v.choices
            delegate: Kirigami.RadioSubtitleDelegate {
                required property var modelData
                Layout.fillWidth: true
                QQC2.ButtonGroup.group: group
                enabled: modelData.enabled
                text: modelData.title
                subtitle: modelData.subtitle
                checked: modelData.id === page.v.selected
                onClicked: page.host.send("daily", modelData.id)
            }
        }
        Note {
            note: page.v.note
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true
            visible: page.custom !== null && page.custom !== undefined
            enabled: page.v.editable

            ColumnLayout {
                Kirigami.FormData.label: page.custom ? page.custom.name.label : ""
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                QQC2.TextField {
                    id: nameField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhNoPredictiveText | Qt.ImhLowercaseOnly | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.send("daily-name", text)
                    onAccepted: page.host.send("continue")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.custom ? page.custom.name_note : null
                }
            }

            ColumnLayout {
                Kirigami.FormData.label: page.custom ? page.custom.size.label : ""
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                QQC2.TextField {
                    id: sizeField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhDigitsOnly | Qt.ImhNoPredictiveText
                    onTextEdited: page.host.send("daily-size", text)
                    onAccepted: page.host.send("continue")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.custom ? page.custom.size_note : null
                }
            }
        }
    }
}
