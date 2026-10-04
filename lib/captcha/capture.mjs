/**
 * Захват капчи FunTime с уже подключённого mineflayer-бота.
 * Не вызывает quit — только собирает PNG.
 */
import { createRequire } from 'module';
import { PNG } from 'pngjs';
import { frameMetaKeys } from './frames.mjs';
import { assembleCaptcha } from './assemble.mjs';
import {
    inspectCaptchaPng,
    isGoodCaptchaPng,
    pickCaptchaWallFrames,
} from './quality.mjs';
import { botSnapshot, formatSnapshot } from './diag.mjs';

const require = createRequire(import.meta.url);
const ItemUtils = require('flayercaptcha/utils/itemUtils.js');
const EntityUtils = require('flayercaptcha/utils/entityUtils.js');

const MIN_FRAMES = 12;

function frameViewDirection(ent) {
    const { horizontalDirection, verticalDirection } = EntityUtils.getViewDirection(
        ent.yaw,
        ent.pitch,
    );
    return verticalDirection || horizontalDirection;
}

/** Короткий хэш байтов карты — ловим reuse mapId с новым содержимым. */
function mapContentTag(buf) {
    if (!buf?.length) return '';
    let h = buf.length >>> 0;
    const step = Math.max(1, (buf.length / 64) | 0);
    for (let i = 0; i < buf.length; i += step) h = (h * 33 + buf[i]) >>> 0;
    h = (h * 33 + buf[buf.length - 1]) >>> 0;
    return h.toString(36);
}

export function captchaFingerprint(frames, mapsRaw = null) {
    return frames
        .map((f) => {
            const tag = mapsRaw ? mapContentTag(mapsRaw.get(f.mapId)) : '';
            return `${f.mapId}:${f.rotate}:${f.coordinate.x},${f.coordinate.y},${f.coordinate.z}:${tag}`;
        })
        .sort()
        .join('|');
}

/** Доля почти чёрных пикселей — дырки от недоехавших тайлов. */
export function blackPixelRatio(pngBuf) {
    const png = PNG.sync.read(pngBuf);
    let black = 0;
    const n = png.width * png.height;
    for (let i = 0; i < png.data.length; i += 4) {
        if (png.data[i] < 8 && png.data[i + 1] < 8 && png.data[i + 2] < 8) black += 1;
    }
    return n ? black / n : 1;
}

export function isCompleteCaptchaPng(pngBuf, maxBlackRatio = 0.04) {
    try {
        return blackPixelRatio(pngBuf) <= maxBlackRatio;
    } catch {
        return false;
    }
}

export { isGoodCaptchaPng, inspectCaptchaPng };

/**
 * Кэш map-байтов на боте.
 * FunTime после wrong шлёт новую капчу (часто теми же mapId) — seq растёт,
 * даже если пакеты прилетели до старта следующего waitForCaptcha.
 */
export function attachMapCache(bot) {
    if (!bot) return new Map();
    bot._kapchaMapCache = bot._kapchaMapCache || new Map();
    bot._kapchaMapSeq = bot._kapchaMapSeq || 0;
    bot._kapchaMapUpdatedAt = bot._kapchaMapUpdatedAt || new Map();
    if (!bot._client || bot._kapchaMapCacheAttached) return bot._kapchaMapCache;
    bot._kapchaMapCacheAttached = true;
    bot._client.on('map', (packet) => {
        const id = packet.mapId ?? packet.itemDamage;
        if (id == null || !packet.data?.length) return;
        const mid = Number(id);
        bot._kapchaMapCache.set(mid, Buffer.from(packet.data));
        bot._kapchaMapSeq = (bot._kapchaMapSeq || 0) + 1;
        bot._kapchaMapUpdatedAt.set(mid, bot._kapchaMapSeq);
        // FunTime шлёт много mapId (капча + декор) — без капа кэш жирнеет вместе с ботом
        while (bot._kapchaMapCache.size > 128) {
            const oldest = bot._kapchaMapCache.keys().next().value;
            if (oldest == null) break;
            bot._kapchaMapCache.delete(oldest);
            bot._kapchaMapUpdatedAt.delete(oldest);
        }
    });
    return bot._kapchaMapCache;
}

/**
 * @param {import('mineflayer').Bot} bot
 * @param {{
 *   quietMs?: number,
 *   maxWaitMs?: number,
 *   minFrames?: number,
 *   prevFingerprint?: string | null,
 *   requireFreshMaps?: boolean,
 *   minMapSeq?: number, // map seq > minMapSeq = свежие (в т.ч. до старта wait)
 *   log?: (...a:any[])=>void
 * }} [opts]
 * @returns {Promise<{ png: Buffer, layout: object, frames: object[], fingerprint: string, quality: object } | null>}
 */
export function waitForCaptcha(bot, opts = {}) {
    const quietMs = opts.quietMs ?? 900;
    const maxWaitMs = opts.maxWaitMs ?? 15_000;
    const minFrames = opts.minFrames ?? MIN_FRAMES;
    const prevFingerprint = opts.prevFingerprint || null;
    // После wrong FunTime шлёт новую капчу; пакеты часто приходят ДО attempt N+1.
    // requireFreshMaps + minMapSeq: свежий = пакет в этом wait ИЛИ seq после wrong.
    const requireFreshMaps = opts.requireFreshMaps === true;
    const minMapSeq = opts.minMapSeq ?? 0;
    const log = opts.log || (() => {});

    const cache = attachMapCache(bot);
    const updatedAt = bot._kapchaMapUpdatedAt || new Map();
    /** @type {Map<number, Buffer>} */
    const mapsRaw = new Map(cache);
    /** mapId → сколько пакетов map пришло за этот wait */
    const freshMapCounts = new Map();
    /** @type {Map<number, object>} */
    const frames = new Map();

    const isFreshMap = (mapId) => {
        if (freshMapCounts.has(mapId)) return true;
        if (minMapSeq > 0 && (updatedAt.get(mapId) || 0) > minMapSeq) return true;
        return false;
    };

    let quietTimer = null;
    let settled = false;
    let mapPackets = 0;
    let mapPacketsEmpty = 0;
    let metaHits = 0;
    let spawnHits = 0;
    const t0 = Date.now();

    function failDiag(reason, extra = '') {
        refreshFramesFromBot();
        const withMaps = [...frames.values()].filter((f) => mapsRaw.has(f.mapId)).length;
        const snap = formatSnapshot(botSnapshot(bot, { mapsRaw, cache }));
        log(
            `waitForCaptcha(${reason}): ${extra || 'fail'} ` +
                `need=${minFrames} trackedFrames=${frames.size} withMaps=${withMaps} ` +
                `mapsRaw=${mapsRaw.size} fresh=${mapPackets} emptyMapPkts=${mapPacketsEmpty} ` +
                `meta=${metaHits} spawnEnt=${spawnHits} ` +
                `waited=${((Date.now() - t0) / 1000).toFixed(1)}s | ${snap}`,
        );
    }

    function readFrameFromEntity(ent) {
        if (!ent || !EntityUtils.isEntityFrame(bot, ent.entityType)) return null;
        const keys = frameMetaKeys(bot);
        const md = ent.metadata || {};
        const item = md[keys.item];
        if (!ItemUtils.isFilledMap(bot, item)) return null;
        const mapId = ItemUtils.getValueOfFilledMap(item);
        if (mapId == null) return null;
        const rotate = Number(md[keys.rotate] ?? 0) & 7;
        return {
            entityFrameId: ent.id,
            mapId: Number(mapId),
            rotate,
            coordinate: {
                x: ent.position.x,
                y: ent.position.y,
                z: ent.position.z,
            },
            viewDirection: frameViewDirection(ent),
            yaw: ent.yaw,
            pitch: ent.pitch,
        };
    }

    function refreshFramesFromBot() {
        for (const ent of Object.values(bot.entities || {})) {
            const row = readFrameFromEntity(ent);
            if (row) frames.set(row.entityFrameId, row);
        }
    }

    function pickFrames() {
        refreshFramesFromBot();
        const withMaps = [...frames.values()].filter((f) => mapsRaw.has(f.mapId));
        if (withMaps.length < minFrames) return null;

        let botEnt = bot.entity;
        let pool = withMaps;
        if (botEnt) {
            const fwd = withMaps.filter(
                (f) => EntityUtils.getFacing(botEnt, f.viewDirection) === 'forward',
            );
            if (fwd.length >= minFrames) pool = fwd;
        }

        const origin = botEnt?.position
            ? {
                  x: botEnt.position.x,
                  y: botEnt.position.y,
                  z: botEnt.position.z,
              }
            : null;

        const wall = pickCaptchaWallFrames(pool, origin, minFrames);
        if (wall?.length >= minFrames) return wall;

        const byDir = new Map();
        for (const f of pool) {
            if (!byDir.has(f.viewDirection)) byDir.set(f.viewDirection, []);
            byDir.get(f.viewDirection).push(f);
        }
        let best = [];
        for (const list of byDir.values()) {
            if (list.length > best.length) best = list;
        }
        if (best.length < minFrames) best = pool;
        if (best.length < minFrames) return null;

        const wall2 = pickCaptchaWallFrames(best, origin, minFrames);
        return wall2?.length >= minFrames ? wall2 : null;
    }

    return new Promise((resolve) => {
        let tick = null;

        const cleanup = () => {
            if (quietTimer) clearTimeout(quietTimer);
            if (tick) clearInterval(tick);
            tick = null;
            bot._client?.removeListener('map', onMap);
            bot._client?.removeListener('entity_metadata', onMeta);
            bot.removeListener('entitySpawn', onSpawn);
            bot.removeListener('kicked', onKicked);
            bot.removeListener('end', onEnd);
        };

        const finish = async (reason) => {
            if (settled) return;
            const best = pickFrames();
            if (!best || best.length < minFrames) {
                if (reason === 'timeout' || reason === 'kicked' || reason === 'end') {
                    settled = true;
                    cleanup();
                    failDiag(reason, 'мало тайлов');
                    resolve(null);
                }
                return;
            }

            const fingerprint = captchaFingerprint(best, mapsRaw);
            if (prevFingerprint && fingerprint === prevFingerprint) {
                if (reason === 'timeout' || reason === 'kicked' || reason === 'end') {
                    settled = true;
                    cleanup();
                    failDiag(reason, 'капча не сменилась');
                    resolve(null);
                }
                return;
            }

            if (requireFreshMaps) {
                const stale = best.filter((f) => !isFreshMap(f.mapId));
                if (stale.length) {
                    log(
                        `waitForCaptcha(${reason}): stale maps ${stale.length}/${best.length} — жду`,
                    );
                    if (reason === 'timeout' || reason === 'kicked' || reason === 'end') {
                        settled = true;
                        cleanup();
                        failDiag(reason, 'stale maps');
                        resolve(null);
                    }
                    return;
                }
            }

            try {
                const { png, layout } = await assembleCaptcha(best, mapsRaw);
                const quality = inspectCaptchaPng(png);
                if (!quality.ok) {
                    log(
                        `waitForCaptcha(${reason}): брак (${quality.reason}) — жду`,
                    );
                    if (reason === 'timeout' || reason === 'kicked' || reason === 'end') {
                        settled = true;
                        cleanup();
                        failDiag(reason, `брак ${quality.reason}`);
                        resolve(null);
                    }
                    return;
                }
                settled = true;
                cleanup();
                log(
                    `waitForCaptcha(${reason}): ok n=${best.length}` +
                        ` seams=${quality.seamMean.toFixed(1)}` +
                        ` maps=${mapsRaw.size} fresh=${mapPackets}`,
                );
                resolve({ png, layout, frames: best, fingerprint, quality });
            } catch (e) {
                log(`waitForCaptcha assemble: ${e.message}`);
                if (reason === 'timeout' || reason === 'kicked' || reason === 'end') {
                    settled = true;
                    cleanup();
                    failDiag(reason, `assemble ${e.message}`);
                    resolve(null);
                }
            }
        };

        const schedule = (why) => {
            if (settled) return;
            if (quietTimer) clearTimeout(quietTimer);
            quietTimer = setTimeout(() => void finish(why), quietMs);
        };

        const onMap = (packet) => {
            const id = packet.mapId ?? packet.itemDamage;
            if (id == null) return;
            if (!packet.data?.length) {
                mapPacketsEmpty += 1;
                return;
            }
            const mid = Number(id);
            const buf = Buffer.from(packet.data);
            mapsRaw.set(mid, buf);
            cache.set(mid, buf);
            freshMapCounts.set(mid, (freshMapCounts.get(mid) || 0) + 1);
            mapPackets += 1;
            schedule('map');
        };

        const onMeta = (packet) => {
            setImmediate(() => {
                const row = readFrameFromEntity(bot.entities?.[packet.entityId]);
                if (row) {
                    metaHits += 1;
                    frames.set(row.entityFrameId, row);
                    schedule('meta');
                }
            });
        };

        const onSpawn = (ent) => {
            const row = readFrameFromEntity(ent);
            if (row) {
                spawnHits += 1;
                frames.set(row.entityFrameId, row);
                schedule('spawn-ent');
            }
        };

        const onKicked = () => void finish('kicked');
        const onEnd = () => void finish('end');

        if (!bot._client) {
            failDiag('no-client', 'нет _client');
            resolve(null);
            return;
        }

        bot._client.on('map', onMap);
        bot._client.on('entity_metadata', onMeta);
        bot.on('entitySpawn', onSpawn);
        bot.once('kicked', onKicked);
        bot.once('end', onEnd);

        refreshFramesFromBot();
        log(
            `waitForCaptcha start: cache=${cache.size} ` +
                `prevFp=${prevFingerprint ? 'yes' : 'no'} | ` +
                formatSnapshot(botSnapshot(bot, { mapsRaw, cache })),
        );
        if (frames.size || mapsRaw.size) schedule('warm');

        tick = setInterval(() => {
            if (settled) return;
            refreshFramesFromBot();
            const withMaps = [...frames.values()].filter((f) => mapsRaw.has(f.mapId)).length;
            log(
                `waitForCaptcha… ${((Date.now() - t0) / 1000).toFixed(0)}s ` +
                    `frames=${frames.size} withMaps=${withMaps} maps=${mapsRaw.size} ` +
                    `fresh=${mapPackets} emptyPkts=${mapPacketsEmpty} ` +
                    `state=${bot._client?.state || '?'}`,
            );
        }, 4_000);

        setTimeout(() => void finish('timeout'), maxWaitMs);
    });
}

/** Случайный «ответ», почти наверняка неверный — чтобы сервер выдал новую капчу. */
export function wrongCaptchaGuess() {
    return String(1000 + Math.floor(Math.random() * 9000));
}
