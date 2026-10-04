/**
 * Диагностика сессии / капчи — что видно боту в момент фейла.
 */
import { createRequire } from 'module';
import { frameMetaKeys } from './frames.mjs';

const require = createRequire(import.meta.url);
const ItemUtils = require('flayercaptcha/utils/itemUtils.js');
const EntityUtils = require('flayercaptcha/utils/entityUtils.js');

const FRAME_NAMES = new Set(['item_frame', 'glow_item_frame', 'ItemFrame']);

/** Текст кика из разных форматов mineflayer / NBT / JSON chat. */
export function kickText(reason) {
    if (reason == null) return '';
    if (typeof reason === 'string') return reason;
    if (typeof reason === 'object') {
        try {
            if (typeof reason.toString === 'function') {
                const s = reason.toString();
                if (s && s !== '[object Object]') return s;
            }
        } catch {
            /* ignore */
        }
        try {
            const extra = reason?.value?.extra?.value?.value;
            if (Array.isArray(extra)) {
                return extra.map((p) => p?.text?.value ?? p?.text ?? '').join('');
            }
        } catch {
            /* ignore */
        }
        if (reason.text) {
            const base = typeof reason.text === 'string' ? reason.text : reason.text?.value || '';
            const extra = Array.isArray(reason.extra)
                ? reason.extra.map((p) => (typeof p === 'string' ? p : p?.text || '')).join('')
                : '';
            if (base || extra) return `${base}${extra}`;
        }
        if (reason.translate) {
            const with_ = Array.isArray(reason.with)
                ? reason.with.map((w) => (typeof w === 'string' ? w : w?.text || JSON.stringify(w))).join(' ')
                : '';
            return `${reason.translate} ${with_}`.trim();
        }
        try {
            return JSON.stringify(reason).slice(0, 400);
        } catch {
            return String(reason);
        }
    }
    return String(reason);
}

function isFrameEntity(bot, ent) {
    if (!ent) return false;
    const name = ent.name || ent.displayName || '';
    if (FRAME_NAMES.has(name)) return true;
    try {
        return EntityUtils.isEntityFrame(bot, ent.entityType);
    } catch {
        return false;
    }
}

/**
 * Снимок мира бота для лога.
 * @param {import('mineflayer').Bot} bot
 * @param {{ mapsRaw?: Map<number, Buffer>, cache?: Map<number, Buffer> }} [extra]
 */
export function botSnapshot(bot, extra = {}) {
    const ents = Object.values(bot?.entities || {});
    const byName = new Map();
    let frames = 0;
    let framesWithMap = 0;
    let framesNoItem = 0;
    const mapIds = [];

    const keys = bot ? frameMetaKeys(bot) : { item: 8, rotate: 9 };

    for (const ent of ents) {
        const name = ent.name || ent.displayName || `type:${ent.entityType}`;
        byName.set(name, (byName.get(name) || 0) + 1);
        if (!isFrameEntity(bot, ent)) continue;
        frames += 1;
        const md = ent.metadata || {};
        const item = md[keys.item];
        if (!item || item.present === false) {
            framesNoItem += 1;
            continue;
        }
        try {
            if (ItemUtils.isFilledMap(bot, item)) {
                const id = ItemUtils.getValueOfFilledMap(item);
                if (id != null) {
                    framesWithMap += 1;
                    mapIds.push(Number(id));
                }
            }
        } catch {
            /* ignore */
        }
    }

    const topNames = [...byName.entries()]
        .sort((a, b) => b[1] - a[1])
        .slice(0, 8)
        .map(([n, c]) => `${n}×${c}`)
        .join(', ');

    const pos = bot?.entity?.position;
    const cache = extra.cache || bot?._kapchaMapCache;
    const mapsRaw = extra.mapsRaw;

    return {
        clientState: bot?._client?.state || 'no-client',
        ended: Boolean(bot?._client?.ended),
        username: bot?.username || '?',
        game: bot?.game?.gameMode ?? bot?.game?.dimension ?? null,
        dimension: bot?.game?.dimension ?? null,
        pos: pos
            ? `${pos.x.toFixed(1)},${pos.y.toFixed(1)},${pos.z.toFixed(1)}`
            : 'no-pos',
        entities: ents.length,
        frames,
        framesWithMap,
        framesNoItem,
        uniqueMapIdsOnFrames: new Set(mapIds).size,
        mapCache: cache?.size ?? 0,
        mapsRaw: mapsRaw?.size ?? cache?.size ?? 0,
        topEntities: topNames || '—',
        health: bot?.health,
        food: bot?.food,
    };
}

export function formatSnapshot(s) {
    return (
        `state=${s.clientState}${s.ended ? '(ended)' : ''} ` +
        `pos=${s.pos} dim=${s.dimension ?? '?'} ` +
        `ents=${s.entities} frames=${s.frames}` +
        `(map=${s.framesWithMap},empty=${s.framesNoItem}) ` +
        `mapIds=${s.uniqueMapIdsOnFrames} cache=${s.mapCache} ` +
        `hp=${s.health ?? '?'} ` +
        `[${s.topEntities}]`
    );
}
