import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import {createController} from './controller.js';

const BUS_NAME = 'com.trafficview.Daemon1';
const OBJECT_PATH = '/com/trafficview/Daemon1';
const IFACE = 'com.trafficview.Daemon1';

function unpackVariant(variant) {
    if (!variant)
        return null;
    try {
        return variant.deep_unpack();
    } catch (error) {
        console.warn(`TrafficView: cannot unpack D-Bus payload: ${error}`);
        return null;
    }
}

function toNumber(value) {
    if (value === null || value === undefined)
        return 0;
    const number = Number(value);
    return Number.isFinite(number) ? number : 0;
}

function normalizeSnapshot(raw) {
    if (!raw || typeof raw !== 'object')
        return null;
    const net = raw.net ?? {};
    const cpu = raw.cpu ?? {};
    const mem = raw.mem ?? {};
    return {
        net: {rx_bps: toNumber(net.rx_bps), tx_bps: toNumber(net.tx_bps)},
        cpu: {total_pct: toNumber(cpu.total_pct)},
        mem: {used_pct: toNumber(mem.used_pct)},
    };
}

class DaemonAdapter {
    constructor(proxy) {
        this._proxy = proxy;
        this._signalId = 0;
    }

    connectSample(handler) {
        this._signalId = this._proxy.connectSignal('Sample', (_proxy, _sender, params) => {
            handler(normalizeSnapshot(unpackVariant(params?.[0])));
        });
        return this._signalId;
    }

    disconnectSample() {
        if (this._signalId) {
            this._proxy.disconnectSignal(this._signalId);
            this._signalId = 0;
        }
    }

    getSnapshot(handler) {
        this._proxy.call('GetSnapshot', null, Gio.DBusCallFlags.NONE, -1, null, (proxy, result) => {
            try {
                handler(normalizeSnapshot(unpackVariant(proxy.call_finish(result))));
            } catch (error) {
                // daemon 未启动或暂停时不覆盖上一有效值。
                console.warn(`TrafficView: GetSnapshot failed: ${error}`);
            }
        });
    }
}

export default class TrafficViewExtension extends Extension {
    enable() {
        if (this._label)
            return;
        this._label = new St.Label({
            style_class: 'trafficview-label',
            text: 'TrafficView: --',
            reactive: true,
            track_hover: true,
        });
        Main.panel._rightBox.insert_child_at_index(this._label, 0);

        this._menu = new PopupMenu.PopupMenu(this._label, 0.5, St.Side.TOP);
        this._menuManager = new PopupMenu.PopupMenuManager(this._label);
        this._menuManager.addMenu(this._menu);
        Main.uiGroup.add_child(this._menu);

        const pauseItem = new PopupMenu.PopupMenuItem('Pause sampling');
        pauseItem.connect('activate', () => this._callDaemon('Pause', new GLib.Variant('(b)', [true])));
        const resumeItem = new PopupMenu.PopupMenuItem('Resume sampling');
        resumeItem.connect('activate', () => this._callDaemon('Pause', new GLib.Variant('(b)', [false])));
        const settingsItem = new PopupMenu.PopupMenuItem('Open settings');
        settingsItem.connect('activate', () => {
            Gio.Subprocess.new(['trafficview', 'config', 'edit'], Gio.SubprocessFlags.NONE);
        });
        this._menu.addMenuItem(pauseItem);
        this._menu.addMenuItem(resumeItem);
        this._menu.addMenuItem(settingsItem);

        this._clickId = this._label.connect('button-press-event', () => {
            this._menu.toggle();
            return Clutter.EVENT_STOP;
        });

        this._watchId = Gio.bus_watch_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._connectDaemon(),
            () => this._disconnectDaemon(),
        );
    }

    disable() {
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }
        this._disconnectDaemon();
        if (this._menu) {
            this._menu.destroy();
            this._menu = null;
        }
        this._menuManager = null;
        if (this._label) {
            if (this._clickId) {
                this._label.disconnect(this._clickId);
                this._clickId = 0;
            }
            this._label.destroy();
            this._label = null;
        }
        this._proxy = null;
    }

    _connectDaemon() {
        if (this._controller)
            return;
        Gio.DBusProxy.new_for_bus(
            Gio.BusType.SESSION,
            Gio.DBusProxyFlags.NONE,
            null,
            BUS_NAME,
            OBJECT_PATH,
            IFACE,
            null,
            (source, result) => {
                try {
                    this._proxy = Gio.DBusProxy.new_for_bus_finish(result);
                    this._controller = createController(new DaemonAdapter(this._proxy), this._label);
                } catch (error) {
                    console.warn(`TrafficView: cannot create D-Bus proxy: ${error}`);
                }
            },
        );
    }

    _disconnectDaemon() {
        if (this._controller) {
            this._controller.destroy();
            this._controller = null;
        }
        this._proxy = null;
        if (this._label)
            this._label.set_text('TrafficView: --');
    }

    _callDaemon(method, body) {
        if (!this._proxy)
            return;
        this._proxy.call(method, body, Gio.DBusCallFlags.NONE, -1, null, () => {});
    }
}
