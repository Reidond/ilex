// The erase confirmation: the one destructive question. Its words are the
// flow's (`view.confirm`); it is open exactly while the flow has it. Cancel is
// first and has the focus when it opens: Tab reaches the destructive answer,
// never the other way round, and Escape is a cancellation. The two answers are
// in the dialog's own content (not Kirigami's footer buttons) so that Enter
// presses them as well as Space and the focus starts on the safe one.
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

Kirigami.PromptDialog {
    id: dialog

    required property var view
    required property var host
    readonly property var c: view.confirm ? view.confirm : null

    title: c ? c.title : ""
    subtitle: c ? c.body : ""
    dialogType: Kirigami.PromptDialog.Warning
    standardButtons: Kirigami.Dialog.NoButton
    closePolicy: QQC2.Popup.CloseOnEscape

    property bool answered: false

    RowLayout {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.smallSpacing
        Item {
            Layout.fillWidth: true
        }
        PressButton {
            id: cancel
            icon.name: "dialog-cancel"
            text: dialog.c ? dialog.c.cancel : ""
            onClicked: dialog.close()
        }
        PressButton {
            icon.name: "edit-delete"
            text: dialog.c ? dialog.c.confirm : ""
            onClicked: {
                dialog.answered = true;
                dialog.host.send("confirm");
                dialog.close();
            }
        }
    }

    onCChanged: {
        if (c) {
            answered = false;
            if (!visible) {
                open();
            }
        } else if (visible) {
            answered = true;
            close();
        }
    }
    onOpened: cancel.forceActiveFocus()
    // Escape, Cancel or any other way it goes away without the erase: not
    // confirmed. After an answer the flow has moved on and ignores this.
    onClosed: {
        if (!answered) {
            host.send("cancel");
        }
        answered = false;
    }
}
