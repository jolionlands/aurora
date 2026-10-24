pragma ComponentBehavior: Bound
// SPDX-License-Identifier: MIT
import QtQuick
import QtQuick.Window
import QtQml.Models
import QtMultimedia
import org.kde.plasma.plasmoid
import org.kde.plasma.plasma5support as PlasmaSupport
import org.kde.taskmanager as TaskManager

WallpaperItem {
    id: root
    readonly property string kind: configuration.Kind
    readonly property bool batteryPaused: configuration.PauseOnBattery
        && power.data["AC Adapter"] !== undefined
        && power.data["AC Adapter"]["Plugged in"] === false
    readonly property bool covered: coverCount > 0
    property int coverCount: 0
    property bool frameReported: false
    property bool progressReported: false
    readonly property bool paused: !visible || configuration.PlaybackPaused || batteryPaused
        || (configuration.PauseWhenCovered && covered)

    PlasmaSupport.DataSource {
        id: power
        engine: "powermanagement"
        connectedSources: ["AC Adapter"]
    }

    TaskManager.VirtualDesktopInfo { id: desktopInfo }
    TaskManager.ActivityInfo { id: activityInfo }

    TaskManager.TasksModel {
        id: tasks
        filterByScreen: true
        screenGeometry: Qt.rect(Screen.virtualX, Screen.virtualY, Screen.width, Screen.height)
        filterByVirtualDesktop: true
        virtualDesktop: desktopInfo.currentDesktop
        filterByActivity: true
        activity: activityInfo.currentActivity
    }
    // Instantiators track actual window state without creating visual items.
    Instantiator {
        model: tasks
        delegate: QtObject {
            required property var model
            property bool covers: !model.IsMinimized && (model.IsFullScreen || model.IsMaximized)
            property bool counted: false
            function update() {
                if (counted === covers) return;
                root.coverCount += covers ? 1 : -1;
                counted = covers;
            }
            onCoversChanged: update()
            Component.onCompleted: update()
            Component.onDestruction: { if (counted) root.coverCount--; }
        }
    }

    Rectangle { anchors.fill: parent; color: "black" }

    Image {
        anchors.fill: parent
        source: root.kind === "image" ? root.configuration.Source : ""
        visible: root.kind === "image"
        fillMode: root.configuration.FillMode
        asynchronous: true
        cache: false
        onStatusChanged: { if (status === Image.Ready) root.loading = false; }
    }
    AnimatedImage {
        anchors.fill: parent
        source: root.kind === "animated" ? root.configuration.Source : ""
        visible: root.kind === "animated"
        fillMode: root.configuration.FillMode
        playing: !root.paused
        cache: false
        onStatusChanged: { if (status === Image.Ready) root.loading = false; }
    }
    AudioOutput {
        id: audio
        muted: root.configuration.Muted
        volume: Math.max(0, Math.min(1, root.configuration.Volume / 100))
    }
    MediaPlayer {
        id: player
        source: root.kind === "video" ? root.configuration.Source : ""
        audioOutput: audio
        videoOutput: video
        loops: MediaPlayer.Infinite
        onSourceChanged: {
            root.frameReported = false;
            root.progressReported = false;
            if (source.toString() !== "" && !root.paused) play();
        }
        onPositionChanged: (position) => {
            if (position >= 1000 && !root.progressReported) {
                root.progressReported = true;
                console.info("Aurora: video position advanced", position, source);
            }
        }
        onMediaStatusChanged: {
            if (mediaStatus === MediaPlayer.LoadedMedia || mediaStatus === MediaPlayer.BufferedMedia) {
                root.loading = false;
                if (!root.paused) play();
            }
        }
        onPlaybackStateChanged: {
            if (playbackState === MediaPlayer.PlayingState)
                console.info("Aurora: video playback started", source);
        }
        onErrorOccurred: (error, message) => {
            root.loading = false;
            console.warn("Aurora: video playback failed", message, source);
        }
    }
    VideoOutput {
        id: video
        anchors.fill: parent
        visible: root.kind === "video"
        fillMode: root.configuration.FillMode === 0 ? VideoOutput.Stretch
            : root.configuration.FillMode === 1 || root.configuration.FillMode === 6
            ? VideoOutput.PreserveAspectFit : VideoOutput.PreserveAspectCrop
    }
    Connections {
        target: video.videoSink
        function onVideoSizeChanged() {
            const size = video.videoSink.videoSize;
            if (size.width > 0 && size.height > 0 && !root.frameReported) {
                root.frameReported = true;
                console.info("Aurora: video frame decoded", size.width, size.height, player.source);
            }
        }
    }
    onPausedChanged: {
        if (paused) player.pause();
        else if (kind === "video") player.play();
    }
    Component.onCompleted: {
        root.loading = false;
        if (kind === "video" && !paused) player.play();
    }
}
