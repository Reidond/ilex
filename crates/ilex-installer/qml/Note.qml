// A line of text under a field or a list: an error, or dimmed advice.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

QQC2.Label {
    property var note: null

    Layout.fillWidth: true
    visible: note !== null && note !== undefined
    text: note ? note.text : ""
    wrapMode: Text.Wrap
    font: Kirigami.Theme.smallFont
    color: note && note.error ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.disabledTextColor
}
