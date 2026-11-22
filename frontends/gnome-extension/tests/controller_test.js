// 纯逻辑测试：不访问 session bus，也不需要 GNOME Shell。
// 可运行：gjs -m frontends/gnome-extension/tests/controller_test.js
// 或（无 gjs 的 CI）：node frontends/gnome-extension/tests/controller_test.js

import {createController, formatSnapshot} from '../controller.js';

function assertEqual(actual, expected, message) {
    if (actual !== expected)
        throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

function assertTrue(value, message) {
    if (!value)
        throw new Error(message);
}

class FakeLabel {
    constructor() {
        this.text = '';
    }

    set_text(text) {
        this.text = text;
    }
}

class FakeProxy {
    constructor(initial) {
        this.initial = initial;
        this.handlers = new Map();
        this.nextId = 1;
        this.disconnected = [];
    }

    connectSample(handler) {
        const id = this.nextId++;
        this.handlers.set(id, handler);
        return id;
    }

    disconnectSample(id) {
        this.disconnected.push(id);
        this.handlers.delete(id);
    }

    getSnapshot(handler) {
        handler(this.initial);
    }

    emit(snapshot) {
        for (const handler of [...this.handlers.values()])
            handler(snapshot);
    }

    get connectedHandlers() {
        return this.handlers.size;
    }
}

function snapshot(rx, tx, cpu, mem) {
    return {
        net: {rx_bps: rx, tx_bps: tx},
        cpu: {total_pct: cpu},
        mem: {used_pct: mem},
    };
}

// 1. 格式化与非法负载
assertEqual(formatSnapshot(snapshot(1024, 2048, 12, 34)), '↓1.0 KB/s ↑2.0 KB/s CPU 12% MEM 34%', 'format');
assertEqual(formatSnapshot({net: {}}), null, 'missing sections are rejected');
assertEqual(formatSnapshot(null), null, 'null is rejected');

// 2. 初始 GetSnapshot 与连续 Sample 更新
const label = new FakeLabel();
const proxy = new FakeProxy(snapshot(1024, 2048, 12, 34));
const controller = createController(proxy, label);
assertTrue(label.text.includes('↓1.0 KB/s'), `initial snapshot applied: ${label.text}`);
proxy.emit(snapshot(2048, 4096, 50, 60));
assertEqual(label.text, '↓2.0 KB/s ↑4.0 KB/s CPU 50% MEM 60%', 'second sample applied');

// 3. 非法负载保留上一有效值，只警告一次
let warnings = 0;
const warningProxy = new FakeProxy(snapshot(1024, 0, 10, 20));
const warningLabel = new FakeLabel();
const warningController = createController(warningProxy, warningLabel, {
    warn: () => {
        warnings++;
    },
});
const validText = warningLabel.text;
warningProxy.emit({net: {rx_bps: 'broken'}});
warningProxy.emit({cpu: {}});
assertEqual(warningLabel.text, validText, 'invalid payload keeps the last value');
assertEqual(warnings, 1, 'only one warning per invalid streak');
warningController.destroy();

// 4. destroy 后断开信号且不再更新
const lastText = label.text;
controller.destroy();
assertEqual(proxy.connectedHandlers, 0, 'signal disconnected');
assertEqual(proxy.disconnected.length, 1, 'disconnect called once');
proxy.emit(snapshot(4096, 8192, 90, 95));
assertEqual(label.text, lastText, 'label frozen after destroy');

console.log('gnome controller tests passed');
