// The network: the wired adapter, what is wrong (or that the registry can be
// reached), the Wi-Fi networks in range with a password field for the chosen
// one, Connect and Refresh. The states, words and rows are the flow's
// (`view.network`). The password is read from Rust (`sendField`: the text is
// not passed through a JavaScript string) and cleared when the flow wipes it.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.ScrollablePage {
    id: page

    required property var view
    required property var host
    readonly property string pageId: "network"
    readonly property var v: view.network

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
            "network-password": passwordField
        }[name];
    }

    // The button has the focus when the page is shown.
    onIsCurrentPageChanged: {
        if (isCurrentPage) {
            Qt.callLater(bar.takeFocus);
        }
    }
    // Continue has the focus once it can be pressed (a cable plugged in, a
    // network joined): Enter goes on.
    readonly property bool canContinue: !!page.v.chrome.next && page.v.chrome.next.enabled
    onCanContinueChanged: {
        if (canContinue && isCurrentPage) {
            Qt.callLater(bar.takeFocus);
        }
    }
    Component.onCompleted: {
        Qt.callLater(bar.takeFocus);
        host.restoreField("network-password", passwordField);
    }
    Connections {
        target: page.host
        function onWipe() {
            passwordField.clear();
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
            QQC2.Label {
                Kirigami.FormData.label: page.v.wired_label
                Layout.fillWidth: true
                wrapMode: Text.Wrap
                text: page.v.wired
            }
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: true
            type: page.v.status.error ? Kirigami.MessageType.Warning : Kirigami.MessageType.Positive
            text: page.v.status.text
        }

        RowLayout {
            Layout.fillWidth: true
            visible: page.v.networks.length > 0 || page.v.refresh.visible
            Kirigami.Heading {
                Layout.fillWidth: true
                level: 2
                text: page.v.wifi_group
            }
            QQC2.Button {
                visible: page.v.refresh.visible
                enabled: page.v.refresh.enabled
                icon.name: "view-refresh"
                text: page.v.refresh.label
                onClicked: page.host.send("refresh")
            }
        }
        QQC2.Label {
            Layout.fillWidth: true
            visible: !!page.v.empty
            wrapMode: Text.Wrap
            text: page.v.empty || ""
        }
        QQC2.ButtonGroup {
            id: group
        }
        Repeater {
            model: page.v.networks
            delegate: Kirigami.RadioSubtitleDelegate {
                required property var modelData
                Layout.fillWidth: true
                QQC2.ButtonGroup.group: group
                enabled: page.v.editable && modelData.available
                text: modelData.id
                subtitle: modelData.subtitle
                checked: modelData.id === page.v.selected
                icon.name: modelData.signal >= 70 ? "network-wireless-signal-excellent" : modelData.signal >= 40 ? "network-wireless-signal-good" : "network-wireless-signal-weak"
                onClicked: page.host.send("network", modelData.id)
            }
        }

        Kirigami.FormLayout {
            Layout.fillWidth: true
            visible: !!page.v.password
            enabled: page.v.editable
            ColumnLayout {
                Kirigami.FormData.label: page.v.password ? page.v.password.label : ""
                Kirigami.FormData.labelAlignment: Qt.AlignTop
                Kirigami.PasswordField {
                    id: passwordField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    inputMethodHints: Qt.ImhSensitiveData | Qt.ImhNoPredictiveText | Qt.ImhNoAutoUppercase
                    onTextEdited: page.host.sendField("network-password", passwordField)
                    onAccepted: page.host.send("connect")
                }
                Note {
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                    note: page.v.password_note
                }
            }
        }
        RowLayout {
            Layout.fillWidth: true
            visible: page.v.networks.length > 0
            QQC2.Button {
                enabled: page.v.connect.enabled
                highlighted: true
                text: page.v.connect.label
                onClicked: page.host.send("connect")
            }
            QQC2.BusyIndicator {
                visible: !!page.v.busy
                running: visible
            }
            QQC2.Label {
                visible: !!page.v.busy
                text: page.v.busy || ""
            }
        }
    }
}
