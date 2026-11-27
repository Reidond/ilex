// A button that Enter presses as well as Space: Qt Quick Controls' own buttons
// take Space only, and the installer is used with the keyboard alone (Enter
// continues, as it does on every other page and in the GNOME edition).
import QtQuick
import QtQuick.Controls as QQC2

QQC2.Button {
    Keys.onReturnPressed: click()
    Keys.onEnterPressed: click()
}
