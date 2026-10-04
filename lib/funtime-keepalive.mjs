/**
 * FunTime keep_alive = i64. После configuration mineflayer/nmp иногда не эмитит
 * packet → штатный keepalive.js не отвечает → сервер кикает.
 *
 * Ставим keepAlive: false в createBot и отвечаем сырым echo i64 с wire.
 * Тап после transfer/compression часто «отваливается» — рехукаем и дублируем
 * ответ через keep_alive event, если парсер всё же сработал.
 *
 * Idle = 0 входящих пакетов (packet+wire) за checkTimeoutInterval.
 * Раньше только warn — зомби play+ground глушил /ah|/balance вечно.
 * Теперь после N idle подряд рвём сокет → орк поднимет чистую сессию.
 */

import { replyKeepAlive, tapClientPackets } from './bot-view/wire.mjs';

/** Опция createBot: выключить встроенный keepalive.js */
export const FUNTIME_KEEPALIVE_BOT_OPTS = {
    keepAlive: false,
};

/** Сколько подряд 90с-idle до end. 1 = рвём после первого настоящего молчания. */
const IDLE_STRIKES_TO_END = 1;

function writeVarInt(n) {
    const bytes = [];
    let v = n >>> 0;
    while (true) {
        if ((v & ~0x7f) === 0) {
            bytes.push(v);
            break;
        }
        bytes.push((v & 0x7f) | 0x80);
        v >>>= 7;
    }
    return Buffer.from(bytes);
}

/** play serverbound keep_alive id (1.21.x) */
const PLAY_KEEP_ALIVE_SERVERBOUND = 0x1b;
const CONFIG_KEEP_ALIVE = 0x04;

function keepAliveIdToBe8(id) {
    if (id == null) return null;
    if (Buffer.isBuffer(id)) {
        if (id.length < 8) return null;
        return id.subarray(0, 8);
    }
    try {
        let big;
        if (typeof id === 'bigint') big = id;
        else if (typeof id === 'number' && Number.isFinite(id)) big = BigInt(Math.trunc(id));
        else if (typeof id === 'string' && id) big = BigInt(id);
        else if (typeof id === 'object') {
            // protodef Long: { high, low } или .toBigInt / .toString
            if (typeof id.toBigInt === 'function') big = id.toBigInt();
            else if (typeof id.toString === 'function' && (id.high != null || id.low != null)) {
                const hi = BigInt(id.high >>> 0);
                const lo = BigInt(id.low >>> 0);
                big = (hi << 32n) | lo;
            } else if (typeof id.toString === 'function') {
                big = BigInt(id.toString());
            } else return null;
        } else return null;
        const buf = Buffer.alloc(8);
        buf.writeBigInt64BE(big);
        return buf;
    } catch {
        return null;
    }
}

/**
 * @param {import('mineflayer').Bot} bot
 * @param {{ checkTimeoutInterval?: number, log?: (msg: string) => void, idleStrikesToEnd?: number }} [opts]
 */
export function installFuntimeKeepAlive(bot, opts = {}) {
    const client = bot?._client;
    if (!client || client._funtimeKeepAliveInstalled) return;
    client._funtimeKeepAliveInstalled = true;

    const checkTimeoutInterval = Number(opts.checkTimeoutInterval) > 0
        ? Number(opts.checkTimeoutInterval)
        : 90_000;
    const idleStrikesToEnd = Number(opts.idleStrikesToEnd) > 0
        ? Number(opts.idleStrikesToEnd)
        : IDLE_STRIKES_TO_END;
    const log = typeof opts.log === 'function' ? opts.log : null;

    let timeout = null;
    let lastEchoKey = '';
    let lastPacketAt = 0;
    let replied = 0;
    let idleStrikes = 0;

    bot._funtimeKaLastPacketAt = 0;
    bot._funtimeKaReplied = 0;
    bot._funtimeKaIdleStrikes = 0;

    const clear = () => {
        if (timeout) {
            clearTimeout(timeout);
            timeout = null;
        }
    };

    /**
     * @param {string} [why]
     * @param {{ touch?: boolean }} [opts] touch=false — только перевзвод таймера
     *   (idle-retry). Раньше arm('idle-retry') сбрасывал lastPacketAt → soft-skip
     *   думал «пакеты идут» и крутил зомби вечно.
     */
    const arm = (why = 'packet', opts = {}) => {
        const touch = opts.touch !== false;
        if (touch) {
            lastPacketAt = Date.now();
            bot._funtimeKaLastPacketAt = lastPacketAt;
            idleStrikes = 0;
            bot._funtimeKaIdleStrikes = 0;
        }
        clear();
        timeout = setTimeout(() => {
            const idle = Date.now() - lastPacketAt;
            idleStrikes += 1;
            bot._funtimeKaIdleStrikes = idleStrikes;
            const msg = `funtime-ka idle ${idle}ms replies=${replied} strike=${idleStrikes}/${idleStrikesToEnd} last=${why}`;
            try { log?.(msg); } catch { /* ignore */ }
            try { console.warn(`[${bot.username || 'bot'}] ⚡ ${msg}`); } catch { /* ignore */ }

            // 0 пакетов N×90с = зомби TCP (play+ground врёт, команды глухие).
            if (idleStrikes >= idleStrikesToEnd && !client.ended) {
                try {
                    console.warn(
                        `[${bot.username || 'bot'}] ⚡ funtime-ka → end (зомби, команды не живут)`,
                    );
                } catch { /* ignore */ }
                try {
                    client.end('funtimeKaIdle');
                } catch { /* ignore */ }
                return;
            }
            if (!client.ended) arm('idle-retry', { touch: false });
        }, checkTimeoutInterval);
        timeout.unref?.();
    };

    const echoPayload = (payload8, replyId) => {
        if (!payload8 || payload8.length < 8) return false;
        const key = `${replyId}:${payload8.toString('hex')}`;
        if (key === lastEchoKey) return true;
        try {
            client.writeRaw(Buffer.concat([writeVarInt(replyId), payload8.subarray(0, 8)]));
            lastEchoKey = key;
            replied += 1;
            bot._funtimeKaReplied = replied;
            arm('keepalive');
            return true;
        } catch {
            return false;
        }
    };

    client.on('end', clear);
    bot.once?.('end', clear);

    // Любой распарсенный пакет = сессия жива (даже если raw-tap отвалился)
    client.on('packet', () => arm('packet'));

    // Path A: сырой wire (когда nmp не эмитит keep_alive из‑за i64)
    tapClientPackets(client, (state, buf) => {
        arm('wire');
        if (replyKeepAlive(client, state, buf)) {
            replied += 1;
            bot._funtimeKaReplied = replied;
            // replyKeepAlive уже writeRaw; запомним ключ грубо по хвосту
            try {
                const payload = buf.subarray(buf.length - 8);
                const replyId = state === 'configuration' ? CONFIG_KEEP_ALIVE : PLAY_KEEP_ALIVE_SERVERBOUND;
                lastEchoKey = `${replyId}:${payload.toString('hex')}`;
            } catch { /* ignore */ }
            arm('keepalive');
        }
    });

    // Path B: если парсер всё же отдал keep_alive — echo i64 сами (не client.write)
    const onKeepAlive = (packet) => {
        const be = keepAliveIdToBe8(packet?.keepAliveId);
        if (!be) {
            arm('ka-event-noparse');
            return;
        }
        const replyId = client.state === 'configuration'
            ? CONFIG_KEEP_ALIVE
            : PLAY_KEEP_ALIVE_SERVERBOUND;
        echoPayload(be, replyId);
    };
    client.on('keep_alive', onKeepAlive);

    // После /an* transfer decompressor часто новый — периодический rehook через tap уже
    // есть на state/compression; плюс сами армим при входе в play.
    const onState = () => {
        if (client.state === 'play' || client.state === 'configuration') arm('state');
    };
    client.on('state', onState);

    // Старт: не ждать первого keepalive, чтобы не висеть вечно без таймаута
    arm('install');
}
