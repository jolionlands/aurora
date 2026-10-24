// SPDX-License-Identifier: MIT
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ColumnLayout {
    property alias cfg_Source: source.text
    property alias cfg_Muted: muted.checked
    property alias cfg_Volume: volume.value
    property alias cfg_PauseOnBattery: battery.checked
    property alias cfg_PauseWhenCovered: covered.checked
    Label { text: "Aurora controls this wallpaper. Use aurora-ctl set <path> to change it."; wrapMode: Text.Wrap; Layout.fillWidth: true }
    TextField { id: source; placeholderText: "file:///path/to/wallpaper"; Layout.fillWidth: true }
    CheckBox { id: muted; text: "Mute video audio" }
    SpinBox { id: volume; from: 0; to: 100 }
    CheckBox { id: battery; text: "Pause on battery power" }
    CheckBox { id: covered; text: "Pause behind fullscreen or maximized windows" }
}
