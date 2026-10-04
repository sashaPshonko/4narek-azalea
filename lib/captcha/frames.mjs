import { Vec3 } from 'vec3';
import { writeFileSync, mkdirSync } from 'fs';
import { join } from 'path';

const FRAME_NAMES = new Set(['item_frame', 'glow_item_frame']);

function lookDir(entity) {
    const yaw = entity.yaw;
    const pitch = entity.pitch;
    return new Vec3(
        -Math.sin(yaw) * Math.cos(pitch),
        -Math.sin(pitch),
        -Math.cos(yaw) * Math.cos(pitch),
    ).normalize();
}

function eyePos(bot) {
    const e = bot.entity;
    return e.position.offset(0, e.eyeHeight ?? 1.62, 0);
}

export function angleToDeg(bot, pos) {
    const eye = eyePos(bot);
    const to = pos.minus(eye);
    const dist = to.norm();
    if (dist < 1e-6) return 0;
    const look = lookDir(bot.entity);
    const cos = Math.min(1, Math.max(-1, look.dot(to.scaled(1 / dist))));
    return (Math.acos(cos) * 180) / Math.PI;
}

function isFrameEntity(ent) {
    if (!ent) return false;
    const name = ent.name || ent.displayName || '';
    return FRAME_NAMES.has(name);
}

/** map_id из components / nbt (1.20.5+). */
export function extractMapId(itemOrNbt) {
    if (itemOrNbt == null) return null;
    if (typeof itemOrNbt === 'number' && Number.isFinite(itemOrNbt)) return itemOrNbt;

    const dmg = itemOrNbt.itemDamage ?? itemOrNbt.metadata ?? null;
    if (dmg != null && Number.isFinite(Number(dmg))) return Number(dmg);

    const nbt = itemOrNbt.nbtData ?? itemOrNbt.nbt ?? itemOrNbt.components ?? itemOrNbt;
    if (Array.isArray(nbt)) {
        const hit = nbt.find((c) => c?.type === 'map_id' || c?.name === 'map_id');
        if (hit?.data != null) return Number(hit.data);
        if (hit?.value != null) return Number(hit.value);
    }
    if (nbt && typeof nbt === 'object') {
        const mapTag = nbt?.value?.map?.value ?? nbt?.map?.value ?? nbt?.map;
        if (mapTag != null) return Number(mapTag);
    }
    return null;
}

function serializeItem(item) {
    if (!item) return null;
    if (typeof item === 'object' && item.present === false) return null;

    const present =
        item.present === true ||
        item.itemId != null ||
        item.itemCount != null;

    const nbt = item.nbtData ?? item.nbt ?? item.components ?? null;
    const mapId = extractMapId(item);

    return {
        present: Boolean(present),
        itemId: item.itemId ?? item.id ?? null,
        itemCount: item.itemCount ?? item.count ?? null,
        itemDamage: item.itemDamage ?? item.metadata ?? null,
        mapId,
        nbt: nbt ?? null,
        rawKeys: Object.keys(item),
    };
}

/** Индексы metadata как в flayercaptcha (1.21.6+ → item=9, rotate=10). */
export function frameMetaKeys(bot) {
    const v = bot?.registry?.version;
    try {
        if (v?.['>=']?.('1.21.6')) return { rotate: 10, item: 9 };
        if (v?.['>=']?.('1.17')) return { rotate: 9, item: 8 };
    } catch {
        /* ignore */
    }
    // 1.21.11 по умолчанию
    return { rotate: 10, item: 9 };
}

function metaNamed(ent, bot) {
    const md = ent.metadata;
    if (!md) return {};
    const keys = frameMetaKeys(bot);
    // mineflayer: metadata — объект { [numericKey]: value }
    if (!Array.isArray(md)) {
        const out = { ...md };
        if (md[keys.item] != null) out.item = md[keys.item];
        if (md[keys.rotate] != null) out.rotation = md[keys.rotate];
        return out;
    }
    if (ent.metadataKeys) {
        return Object.fromEntries(md.map((v, i) => [ent.metadataKeys[i] ?? i, v]));
    }
    return Object.fromEntries(md.map((v, i) => [i, v]));
}

function metaItem(ent, bot) {
    const named = metaNamed(ent, bot);
    if (named.item != null) return named.item;
    const md = ent.metadata;
    if (md && typeof md === 'object' && !Array.isArray(md)) {
        const keys = frameMetaKeys(bot);
        if (md[keys.item] != null) return md[keys.item];
    }
    if (Array.isArray(md)) {
        for (const v of md) {
            if (
                v &&
                typeof v === 'object' &&
                ('itemId' in v || 'present' in v || 'nbtData' in v || 'components' in v)
            ) {
                return v;
            }
        }
    }
    return ent.heldItem ?? null;
}

/** Поворот предмета в рамке: 0..7. */
function metaRotation(ent, bot) {
    const named = metaNamed(ent, bot);
    let r = named.rotation;
    if (r == null) {
        const keys = frameMetaKeys(bot);
        const md = ent.metadata;
        if (md && typeof md === 'object') r = md[keys.rotate];
    }
    if (r == null) return 0;
    if (typeof r === 'object' && r !== null) {
        if (r.value != null) return Number(r.value) & 7;
        if (typeof r.toNumber === 'function') return r.toNumber() & 7;
    }
    const n = Number(r);
    return Number.isFinite(n) ? n & 7 : 0;
}

/**
 * Стена спереди по взгляду: доминирующая ось (x|z),
 * плоскость с наибольшим числом рамок впереди бота.
 */
export function selectFrontWall(frames, botPos, look) {
    const axis = Math.abs(look.x) >= Math.abs(look.z) ? 'x' : 'z';
    const sign = look[axis] >= 0 ? 1 : -1;
    const origin = botPos[axis];

    const ahead = frames.filter((f) => (f[axis] - origin) * sign > 0.4);
    if (!ahead.length) return { frames: [], axis, wallCoord: null };

    const counts = new Map();
    for (const f of ahead) {
        const key = Math.round(f[axis]);
        counts.set(key, (counts.get(key) || 0) + 1);
    }

    let wallCoord = null;
    let bestN = -1;
    for (const [k, n] of counts) {
        // при равенстве — дальняя плоскость (ближе к «стене капчи»)
        if (n > bestN || (n === bestN && (wallCoord == null || (k - origin) * sign > (wallCoord - origin) * sign))) {
            bestN = n;
            wallCoord = k;
        }
    }

    const wall = ahead
        .filter((f) => Math.round(f[axis]) === wallCoord)
        .sort((a, b) => b.y - a.y || a.x - b.x || a.z - b.z);

    return { frames: wall, axis, wallCoord };
}

export function mapIdsFromFrames(frames) {
    const ids = new Set();
    for (const f of frames) {
        const id = f?.item?.mapId ?? extractMapId(f?.item);
        if (id != null && Number.isFinite(Number(id))) ids.add(Number(id));
    }
    return ids;
}

export function collectFrames(bot, { maxDist = 16 } = {}) {
    const eye = eyePos(bot);
    const look = lookDir(bot.entity);
    const all = [];

    for (const ent of Object.values(bot.entities)) {
        if (!isFrameEntity(ent)) continue;
        const pos = ent.position;
        const dist = eye.distanceTo(pos);
        if (dist > maxDist) continue;
        const item = serializeItem(metaItem(ent, bot));
        const rotation = metaRotation(ent, bot);
        const metaKeys = frameMetaKeys(bot);
        const rawMeta = ent.metadata && typeof ent.metadata === 'object'
            ? {
                itemKey: metaKeys.item,
                rotateKey: metaKeys.rotate,
                rotateRaw: ent.metadata[metaKeys.rotate],
                itemPresent: ent.metadata[metaKeys.item] != null,
            }
            : null;
        all.push({
            id: ent.id,
            name: ent.name,
            x: pos.x,
            y: pos.y,
            z: pos.z,
            dist: Number(dist.toFixed(3)),
            angleDeg: Number(angleToDeg(bot, pos).toFixed(2)),
            yaw: ent.yaw,
            pitch: ent.pitch,
            /** 0..7 — поворот карты в рамке (MC: ×45°; flayercaptcha рисует ×90°) */
            rotation,
            rotationDeg: rotation * 45,
            meta: rawMeta,
            item,
        });
    }

    all.sort((a, b) => a.dist - b.dist);

    const botPos = {
        x: bot.entity.position.x,
        y: bot.entity.position.y,
        z: bot.entity.position.z,
    };
    const { frames: front, axis, wallCoord } = selectFrontWall(all, botPos, look);
    const frontMapIds = [...mapIdsFromFrames(front)];

    return {
        bot: {
            username: bot.username,
            x: botPos.x,
            y: botPos.y,
            z: botPos.z,
            yaw: bot.entity.yaw,
            pitch: bot.entity.pitch,
            eye: { x: eye.x, y: eye.y, z: eye.z },
            look: { x: look.x, y: look.y, z: look.z },
        },
        wall: { axis, coord: wallCoord },
        counts: { all: all.length, front: front.length },
        front,
        /** @deprecated alias */
        inFront: front,
        all,
        frontMapIds,
    };
}

export function saveDump(dir, payload) {
    mkdirSync(dir, { recursive: true });
    const stamp = new Date().toISOString().replace(/[:.]/g, '-');
    const file = join(dir, `frames-${stamp}.json`);
    writeFileSync(file, JSON.stringify(payload, null, 2), 'utf8');
    return file;
}
