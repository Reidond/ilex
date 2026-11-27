// A line across the top of a page, as the flow says it (`Banner`: how loud,
// the text, the button's label when asking again can mend it).
import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Kirigami.InlineMessage {
    id: message

    required property var host
    property var banner: null

    readonly property var kinds: ({
            "info": Kirigami.MessageType.Information,
            "warning": Kirigami.MessageType.Warning,
            "error": Kirigami.MessageType.Error
        })

    Layout.fillWidth: true
    position: Kirigami.InlineMessage.Position.Header
    visible: banner !== null && banner !== undefined
    type: banner ? kinds[banner.kind] : Kirigami.MessageType.Information
    text: banner ? banner.text : ""
    actions: [
        Kirigami.Action {
            visible: message.banner !== null && message.banner !== undefined && !!message.banner.retry
            text: message.banner && message.banner.retry ? message.banner.retry : ""
            onTriggered: message.host.send("probe")
        }
    ]
}
