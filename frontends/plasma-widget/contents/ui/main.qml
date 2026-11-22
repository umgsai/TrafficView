import QtQuick
import QtQuick.Layouts
import QtDBus
import org.kde.plasma.plasmoid
import org.kde.plasma.components as PlasmaComponents

PlasmoidItem {
    id: root

    property var snapshot: null
    property bool connected: false
    property string iface: ""

    function formatRate(bytesPerSec) {
        const units = ["B", "KB", "MB", "GB", "TB"];
        let value = Number(bytesPerSec);
        if (!Number.isFinite(value) || value < 0)
            return "--";
        if (value < 1024)
            return Math.round(value) + " B/s";
        let unit = 0;
        while (value >= 1024 && unit < units.length - 1) {
            value /= 1024;
            unit++;
        }
        return value.toFixed(1) + " " + units[unit] + "/s";
    }

    function compactText() {
        if (!snapshot)
            return "TrafficView: --";
        const parts = [];
        if (Plasmoid.configuration.showDownload)
            parts.push("↓" + formatRate(snapshot.net.rx_bps));
        if (Plasmoid.configuration.showUpload)
            parts.push("↑" + formatRate(snapshot.net.tx_bps));
        if (Plasmoid.configuration.showCpu)
            parts.push("CPU " + Math.round(snapshot.cpu.total_pct) + "%");
        return parts.join(" ");
    }

    function applySnapshot(payload) {
        if (!payload || !payload.net || !payload.cpu || !payload.mem) {
            connected = false;
            return;
        }
        snapshot = payload;
        iface = payload.net.iface ?? "";
        connected = true;
    }

    function refresh() {
        try {
            const reply = bus.call("GetSnapshot", []);
            applySnapshot(reply);
        } catch (error) {
            connected = false;
            snapshot = null;
        }
    }

    function callDaemon(method, args) {
        try {
            bus.call(method, args);
        } catch (error) {
            connected = false;
        }
    }

    DBusInterface {
        id: bus
        service: "com.trafficview.Daemon1"
        path: "/com/trafficview/Daemon1"
        iface: "com.trafficview.Daemon1"
        signals: ["Sample", "ConfigChanged"]
        onSample: function (payload) {
            root.applySnapshot(payload);
        }
        onConfigChanged: root.refresh()
        Component.onCompleted: root.refresh()
    }

    Plasmoid.compactRepresentation: CompactRepresentation {
        item: root
    }

    Plasmoid.fullRepresentation: ColumnLayout {
        spacing: 4
        PlasmaComponents.Label {
            text: "Download  " + (root.snapshot ? root.formatRate(root.snapshot.net.rx_bps) : "--")
        }
        PlasmaComponents.Label {
            text: "Upload    " + (root.snapshot ? root.formatRate(root.snapshot.net.tx_bps) : "--")
        }
        PlasmaComponents.Label {
            text: "CPU       " + (root.snapshot ? Math.round(root.snapshot.cpu.total_pct) + "%" : "--")
        }
        PlasmaComponents.Label {
            text: "Memory    " + (root.snapshot ? Math.round(root.snapshot.mem.used_pct) + "%" : "--")
        }
        PlasmaComponents.Label {
            text: "Interface " + (root.iface || "--")
        }
        PlasmaComponents.Label {
            text: "Sample ts " + (root.snapshot ? root.snapshot.ts_ms + " ms" : "--")
        }
        PlasmaComponents.Button {
            text: "Pause sampling"
            onClicked: root.callDaemon("Pause", [true])
        }
        PlasmaComponents.Button {
            text: "Resume sampling"
            onClicked: root.callDaemon("Pause", [false])
        }
    }
}
