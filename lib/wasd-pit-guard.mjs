/**
 * Защита слепого WASD от провала в яму (anti-AFK без shop-map).
 * Карта walk ямы режет сама; здесь — только blockAt под/перед ногами.
 */

import { Vec3 } from 'vec3';

/** Только почти тот же уровень — ступень −1 уже риск у края хаба. */
const MAX_STEP_DOWN = 0.55;
const MAX_STEP_UP = 1.01;
/** Ближе + дальше: инерция после отпускания ключа ещё несёт. */
const AHEAD_DISTS = [0.35, 0.55, 0.95, 1.35, 1.75, 2.15];
const SIDE = 0.32;
const WASD_KEYS = ['forward', 'left', 'back', 'right'];

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function isAirLike(block) {
    if (!block) return true;
    const n = block.name || '';
    if (n === 'air' || n === 'cave_air' || n === 'void_air' || n === 'light' || n === 'snow') {
        return true;
    }
    if (
        n.endsWith('_carpet')
        || n.includes('flower')
        || n === 'short_grass'
        || n === 'tall_grass'
        || n === 'fern'
        || n === 'large_fern'
        || n === 'torch'
        || n === 'wall_torch'
        || n === 'soul_torch'
    ) {
        return true;
    }
    return block.boundingBox === 'empty';
}

function isStandableFloor(block) {
    if (!block || isAirLike(block)) return false;
    const n = block.name || '';
    if (n === 'water' || n === 'lava' || n === 'bubble_column') return false;
    if (n.includes('magma') || n === 'fire' || n === 'soul_fire') return false;
    if (n.includes('leaves') || n.includes('vine') || n.includes('azalea')) return false;
    return (
        block.boundingBox === 'block'
        || n.includes('slab')
        || n.includes('stairs')
        || n.includes('carpet')
    );
}

function blockAt(bot, x, y, z) {
    try {
        return bot.blockAt(new Vec3(x, y, z));
    } catch {
        return null;
    }
}

function lookXZ(yaw) {
    return { x: -Math.sin(yaw), z: -Math.cos(yaw) };
}

/** Направление WASD относительно yaw (голова не крутится). */
export function keyDirXZ(yaw, key) {
    const f = lookXZ(yaw);
    const left = { x: f.z, z: -f.x };
    if (key === 'forward') return f;
    if (key === 'back') return { x: -f.x, z: -f.z };
    if (key === 'left') return left;
    if (key === 'right') return { x: -left.x, z: -left.z };
    return f;
}

/**
 * Высота ног, если в столбце (x,z) есть пол не ниже preferFeetY − maxDown
 * и не выше preferFeetY + maxUp. Иначе null (яма / обрыв / неизвестно).
 */
function probeFeetY(bot, x, z, preferFeetY, {
    maxDown = MAX_STEP_DOWN,
    maxUp = MAX_STEP_UP,
} = {}) {
    const ix = Math.floor(x);
    const iz = Math.floor(z);
    const y0 = Math.floor(preferFeetY + 0.05);
    // пол под ногами: block y = feetY - 1
    const yLo = y0 - Math.ceil(maxDown) - 1;
    const yHi = y0 + Math.ceil(maxUp);
    for (let floorY = yHi; floorY >= yLo; floorY--) {
        const floor = blockAt(bot, ix, floorY, iz);
        if (!isStandableFloor(floor)) continue;
        const feet = blockAt(bot, ix, floorY + 1, iz);
        const head = blockAt(bot, ix, floorY + 2, iz);
        if (!isAirLike(feet) || !isAirLike(head)) continue;
        const feetY = floorY + 1;
        if (feetY < preferFeetY - maxDown) continue;
        if (feetY > preferFeetY + maxUp) continue;
        return feetY;
    }
    return null;
}

/** Стоим на полу (не в полёте над ямой). */
export function isStandingOnFloor(bot) {
    const ent = bot?.entity;
    const pos = ent?.position;
    if (!pos) return false;
    // FunTime: чанки ещё пустые → blockAt=null → ложный «нет пола»,
    // пока сервер уже шлёт onGround. Для grounding/AFK доверяем флагу.
    if (ent.onGround) return true;
    if (ent.isInWater) return true;
    return probeFeetY(bot, pos.x, pos.z, pos.y, { maxDown: 1.25, maxUp: 0.6 }) != null;
}

/**
 * Клавиша ведёт к обрыву/яме глубже 1 блока (или в непрогруженный столб → осторожно skip).
 * @returns {boolean} true = опасно, жать нельзя
 */
/** true = идти в dir (xz unit) опасно. */
export function dirWouldFall(bot, dir) {
    const ent = bot?.entity;
    if (!ent?.position || !dir) return true;
    const pos = ent.position;
    const len = Math.hypot(dir.x || 0, dir.z || 0);
    if (!(len > 1e-6)) return true;
    const nx = dir.x / len;
    const nz = dir.z / len;
    const sideX = -nz * SIDE;
    const sideZ = nx * SIDE;

    for (const dist of AHEAD_DISTS) {
        const samples = [
            [pos.x + nx * dist, pos.z + nz * dist],
            [pos.x + nx * dist + sideX, pos.z + nz * dist + sideZ],
            [pos.x + nx * dist - sideX, pos.z + nz * dist - sideZ],
        ];
        for (const [sx, sz] of samples) {
            const feetY = probeFeetY(bot, sx, sz, pos.y);
            if (feetY == null) return true;
        }
    }
    return false;
}

export function keyWouldFall(bot, key) {
    const ent = bot?.entity;
    if (!ent?.position || !Number.isFinite(ent.yaw)) return true;
    return dirWouldFall(bot, keyDirXZ(ent.yaw, key));
}

/** forward при данном yaw опасен? */
export function yawWouldFall(bot, yaw) {
    if (!Number.isFinite(yaw)) return true;
    return dirWouldFall(bot, lookXZ(yaw));
}

/**
 * Как далеко по yaw ещё есть пол (шаг 0.4, до maxDist).
 * Учитывает яму впереди — для планирования длины шага anti-AFK.
 * @returns {number} безопасная дистанция в блоках (0 если сразу плохо)
 */
export function maxSafeWalkDist(bot, yaw, maxDist = 5) {
    const ent = bot?.entity;
    if (!ent?.position || !Number.isFinite(yaw)) return 0;
    if (!isStandingOnFloor(bot)) return 0;
    const dir = lookXZ(yaw);
    const pos = ent.position;
    const cap = Math.max(0.5, Math.min(12, maxDist));
    let lastOk = 0;
    for (let dist = 0.4; dist <= cap + 1e-6; dist += 0.4) {
        const samples = [
            [pos.x + dir.x * dist, pos.z + dir.z * dist],
            [pos.x + dir.x * dist - dir.z * SIDE, pos.z + dir.z * dist + dir.x * SIDE],
            [pos.x + dir.x * dist + dir.z * SIDE, pos.z + dir.z * dist - dir.x * SIDE],
        ];
        for (const [sx, sz] of samples) {
            if (probeFeetY(bot, sx, sz, pos.y) == null) {
                return lastOk;
            }
        }
        lastOk = dist;
    }
    return lastOk;
}

/**
 * Подобрать yaw, куда можно жать forward (сначала малые повороты — как мышь).
 * @returns {number|null}
 */
export function pickSafeForwardYaw(bot, preferYaw = null) {
    const ent = bot?.entity;
    if (!ent || !Number.isFinite(ent.yaw)) return null;
    if (!isStandingOnFloor(bot)) return null;
    const base = Number.isFinite(preferYaw) ? preferYaw : ent.yaw;
    // малые → крупные; знак чередуем, чтобы не крутить всегда в одну сторону
    const offsets = [
        0,
        0.15, -0.15, 0.35, -0.35, 0.55, -0.55,
        0.85, -0.85, 1.2, -1.2, 1.7, -1.7,
        2.2, -2.2, Math.PI, -Math.PI,
    ];
    for (const off of offsets) {
        const y = base + off;
        if (!yawWouldFall(bot, y)) return y;
    }
    return null;
}

export function clearWasd(bot) {
    if (!bot?.setControlState) return;
    for (const key of [...WASD_KEYS, 'jump', 'sprint']) {
        try {
            bot.setControlState(key, false);
        } catch {
            /* ignore */
        }
    }
}

export function horizSpeed(bot) {
    const v = bot?.entity?.velocity;
    if (!v) return Infinity;
    return Math.hypot(v.x || 0, v.z || 0);
}

/** Сколько из 4 направлений не ведут в яму. */
export function countSafeWasdKeys(bot, keys = WASD_KEYS) {
    if (!isStandingOnFloor(bot)) return 0;
    let n = 0;
    for (const key of keys) {
        if (!keyWouldFall(bot, key)) n++;
    }
    return n;
}

/**
 * У края ямы / на узком карнизе: меньше 3 безопасных направлений.
 * Blind WASD тут не жмём — только стоим (или аварийный warp снаружи).
 */
export function isNearPitEdge(bot) {
    if (!isStandingOnFloor(bot)) return true;
    return countSafeWasdKeys(bot) < 3;
}

/**
 * После отпускания у края: погасить инерцию, дождаться пола.
 * @returns {Promise<boolean>} true = стоим на полу почти без скорости
 */
export async function awaitSettledOnFloor(bot, {
    maxMs = 1800,
    speedMax = 0.06,
    shouldAbort = null,
} = {}) {
    clearWasd(bot);
    const t0 = Date.now();
    while (Date.now() - t0 < maxMs) {
        if (typeof shouldAbort === 'function' && shouldAbort()) return false;
        if (isStandingOnFloor(bot) && horizSpeed(bot) <= speedMax) return true;
        await sleep(40);
    }
    return isStandingOnFloor(bot) && horizSpeed(bot) <= speedMax * 2;
}

/**
 * Следующая безопасная клавиша из карусели, либо null если все 4 ведут в яму.
 * @param {{ keyIndex: number }} st
 */
export function nextSafeWasdKey(bot, st, keys = WASD_KEYS) {
    if (!isStandingOnFloor(bot)) return null;
    if (isNearPitEdge(bot)) return null;
    for (let i = 0; i < keys.length; i++) {
        const key = keys[st.keyIndex % keys.length];
        st.keyIndex = (st.keyIndex + 1) % keys.length;
        if (!keyWouldFall(bot, key)) return key;
    }
    return null;
}
