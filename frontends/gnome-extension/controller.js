// D-Bus 订阅与 UI 更新的可注入控制器：不 import 任何 GI 模块，便于在 GJS/Node 中单测。

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

export function formatRate(bytesPerSec) {
    let value = Number(bytesPerSec);
    if (!Number.isFinite(value) || value < 0)
        return '--';
    if (value < 1024)
        return `${Math.round(value)} B/s`;
    let unit = 0;
    while (value >= 1024 && unit < UNITS.length - 1) {
        value /= 1024;
        unit++;
    }
    return `${value.toFixed(1)} ${UNITS[unit]}/s`;
}

export function formatSnapshot(snapshot) {
    if (!snapshot || typeof snapshot !== 'object')
        return null;
    const { net, cpu, mem } = snapshot;
    if (!net || !cpu || !mem)
        return null;
    const rx = Number(net.rx_bps);
    const tx = Number(net.tx_bps);
    const cpuPct = Number(cpu.total_pct);
    const memPct = Number(mem.used_pct);
    if (![rx, tx, cpuPct, memPct].every(Number.isFinite))
        return null;
    return `↓${formatRate(rx)} ↑${formatRate(tx)} CPU ${Math.round(cpuPct)}% MEM ${Math.round(memPct)}%`;
}

/**
 * @param proxy 需实现 connectSample(cb)->id、disconnectSample(id)、getSnapshot(cb)
 * @param label 需实现 set_text(text)
 */
export function createController(proxy, label, options = {}) {
    const warn = options.warn ?? ((message) => console.warn(message));
    let destroyed = false;
    let warned = false;

    const apply = (snapshot) => {
        if (destroyed)
            return;
        const text = formatSnapshot(snapshot);
        if (text === null) {
            if (!warned) {
                warned = true;
                warn('TrafficView: ignored an invalid Sample payload');
            }
            return;
        }
        warned = false;
        label.set_text(text);
    };

    const signalId = proxy.connectSample(apply);
    proxy.getSnapshot(apply);

    return {
        destroy() {
            if (destroyed)
                return;
            destroyed = true;
            if (signalId !== null && signalId !== undefined)
                proxy.disconnectSample(signalId);
        },
    };
}
