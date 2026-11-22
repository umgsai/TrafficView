import QtQuick
import org.kde.plasma.components as PlasmaComponents

Item {
    id: compact

    property var item: null

    implicitWidth: row.implicitWidth
    implicitHeight: row.implicitHeight

    Row {
        id: row
        spacing: 4
        PlasmaComponents.Label {
            text: compact.item ? compact.item.compactText() : "TrafficView: --"
            opacity: compact.item && compact.item.connected ? 1.0 : 0.6
        }
    }
}
