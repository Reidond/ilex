import QtQuick
import QtQuick.Window

Window {
    width: 1000; height: 680; visible: true
    title: "ilex native WHPX / Hyprland Qt Quick"
    color: "#172434"
    property double started: Date.now()
    property double previous: 0
    property var intervals: []
    property int frames: 0
    onActiveChanged: if (active) console.log(JSON.stringify({qt_ready: true}))
    onFrameSwapped: {
        const now = Date.now()
        if (previous && now - started > 2000) intervals.push(now - previous)
        previous = now
        frames++
    }
    Rectangle {
        width: 180; height: 180; radius: 30
        anchors.centerIn: parent
        color: "#b4f08a"
        NumberAnimation on rotation { from: 0; to: 360; duration: 1800; loops: Animation.Infinite }
        SequentialAnimation on scale {
            loops: Animation.Infinite
            NumberAnimation { to: 1.6; duration: 1000 }
            NumberAnimation { to: 0.7; duration: 1000 }
        }
    }
    Text { anchors.horizontalCenter: parent.horizontalCenter; y: 40; text: "Hyprland · Qt Quick · OpenGL / virgl"; color: "white"; font.pixelSize: 27 }
    TextInput {
        id: source
        x: 35; y: parent.height - 95; width: parent.width - 70; height: 35
        color: "white"; font.pixelSize: 18; activeFocusOnTab: true
        text: "ilex T4 Qt guest → host Привіт 世界 🚀 ✓"
        KeyNavigation.tab: receiver
        Component.onCompleted: forceActiveFocus()
    }
    TextInput {
        id: receiver
        x: 35; y: parent.height - 50; width: parent.width - 70; height: 35
        color: "white"; font.pixelSize: 18; activeFocusOnTab: true
        // A failed bridge must not expose the previous host selection in an
        // acceptance screenshot. Only the boolean assertion goes to the log.
        echoMode: TextInput.Password
        KeyNavigation.tab: source
        onTextChanged: console.log(JSON.stringify({qt_host_to_guest: text === "ilex T4 Qt host → guest Україна 日本語 🙂 ✓"}))
    }
    Timer {
        interval: 90000; running: true
        onTriggered: {
            intervals.sort((a,b) => a-b)
            const sum = intervals.reduce((a,b) => a+b, 0)
            console.log(JSON.stringify({frames: frames, duration_ms: Date.now()-started, samples: intervals.length, mean_ms: sum/intervals.length, median_ms: intervals[Math.floor(intervals.length*.5)], p95_ms: intervals[Math.floor(intervals.length*.95)], p99_ms: intervals[Math.floor(intervals.length*.99)]}))
            Qt.quit()
        }
    }
}
