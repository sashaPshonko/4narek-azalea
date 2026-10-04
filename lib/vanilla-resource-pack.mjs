/**
 * Resource pack как у ванили 1.20.3+:
 * ACCEPTED → пауза «скачивания» → DOWNLOADED → короткая пауза → SUCCESSFULLY_LOADED.
 * Не ACCEPTED+LOADED в один тик.
 */

export const RP_STATUS = {
    SUCCESSFULLY_LOADED: 0,
    DECLINED: 1,
    FAILED_DOWNLOAD: 2,
    ACCEPTED: 3,
    DOWNLOADED: 4,
};

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function rndInt(min, max) {
    return min + Math.floor(Math.random() * (max - min + 1));
}

/**
 * @param {(name: string, params: object) => void} write  client.write / origWrite
 * @param {{ uuid: string, url?: string, log?: (msg: string) => void, shouldAbort?: () => boolean }} opts
 */
export async function acceptResourcePackVanilla(write, opts = {}) {
    const { uuid, url = '', log = null, shouldAbort = null } = opts;
    if (!uuid || typeof write !== 'function') return;

    const abort = () => (typeof shouldAbort === 'function' ? shouldAbort() : false);

    write('resource_pack_receive', { uuid, result: RP_STATUS.ACCEPTED });

    // Ваниль качает по HTTP; FunTime-паки обычно небольшие, но не мгновенные.
    // 2.5–7.5с + чуть дольше на «тяжёлый» url.
    const urlBias = Math.min(2000, String(url).length * 2);
    const downloadMs = rndInt(2500, 7500) + urlBias;
    log?.(`resource pack → download ~${(downloadMs / 1000).toFixed(1)}с`);
    const downloadUntil = Date.now() + downloadMs;
    while (Date.now() < downloadUntil) {
        if (abort()) return;
        await sleep(Math.min(200, downloadUntil - Date.now()));
    }
    if (abort()) return;

    write('resource_pack_receive', { uuid, result: RP_STATUS.DOWNLOADED });

    const applyMs = rndInt(180, 750);
    const applyUntil = Date.now() + applyMs;
    while (Date.now() < applyUntil) {
        if (abort()) return;
        await sleep(Math.min(100, applyUntil - Date.now()));
    }
    if (abort()) return;

    write('resource_pack_receive', { uuid, result: RP_STATUS.SUCCESSFULLY_LOADED });
    log?.('resource pack → loaded');
}
