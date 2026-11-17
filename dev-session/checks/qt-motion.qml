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
    Timer {
        interval: 60000; running: true
        onTriggered: {
            intervals.sort((a,b) => a-b)
            const sum = intervals.reduce((a,b) => a+b, 0)
            console.log(JSON.stringify({frames: frames, duration_ms: Date.now()-started, samples: intervals.length, mean_ms: sum/intervals.length, median_ms: intervals[Math.floor(intervals.length*.5)], p95_ms: intervals[Math.floor(intervals.length*.95)], p99_ms: intervals[Math.floor(intervals.length*.99)]}))
            Qt.quit()
        }
    }
}
