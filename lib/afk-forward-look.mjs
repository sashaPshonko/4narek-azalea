/**
 * Anti-AFK: паттерн записи пилота (motion-records wire).
 *
 * Эталон: один длинный W, look на ходу, player_input редко (press/release).
 * Look — force=true пакетом, но Δyaw только целыми GCD-шагами мыши
 * (без snap в точку каждый тик).
 *
 * @returns {Promise<number>} пройденная дистанция (xz); 0 если не пошли.
 */
import {
    awaitSettledOnFloor,
    clearWasd,
    horizSpeed,
    isStandingOnFloor,
    maxSafeWalkDist,
    pickSafeForwardYaw,
    yawWouldFall,
} from './wasd-pit-guard.mjs';

const TOTAL_DIST_MIN = 28;
const TOTAL_DIST_MAX = 55;
const SESSION_MS_MAX = 22_000;

const RETARGET_EVERY_MS_MIN = 1_600;
const RETARGET_EVERY_MS_MAX = 3_500;
const RETARGET_DELTA_MIN = 0.20;
const RETARGET_DELTA_MAX = 0.85;

const TICK_MS = 50;
const PIT_CHECK_EVERY_MS = 280;
const STUCK_MOVE_MIN = 0.15;
const STUCK_CHECK_MS = 700;
/** Минимальный «коридор» вперёд, иначе W упрётся в край/стену с walked=0. */
const AHEAD_MIN_SAFE = 1.8;
const AHEAD_START_MIN = 2.4;

const PITCH_IDLE_MIN = -0.20;
const PITCH_IDLE_MAX = 0.10;

const FLOOR_WAIT_MS = 1_200;
/** Vanilla 100% mouse step — как walk-route / осмотр. */
const LOOK_GCD_STEP = 0.15 * (Math.PI / 180);
const YAW_DEADBAND = 0.046;

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function rnd(min, max) {
    return min + Math.random() * (max - min);
}

function rndInt(min, max) {
    return min + Math.floor(Math.random() * (max - min + 1));
}

function deltaYaw(a, b) {
    let d = (a - b) % (Math.PI * 2);
    if (d > Math.PI) d -= Math.PI * 2;
    if (d < -Math.PI) d += Math.PI * 2;
    return d;
}

function normalizeAngle(a) {
    while (a > Math.PI) a -= 2 * Math.PI;
    while (a < -Math.PI) a += 2 * Math.PI;
    return a;
}

function clampPitch(p) {
    return Math.max(-0.85, Math.min(0.55, p));
}

function horizDist(a, b) {
    if (!a || !b) return 0;
    return Math.hypot((b.x - a.x) || 0, (b.z - a.z) || 0);
}

/** На ходу: onGround ИЛИ слабый floor-probe (не рвём W из‑за мерцания). */
function canKeepWalking(bot) {
    const ent = bot?.entity;
    if (!ent?.position) return false;
    if (ent.onGround) return true;
    if (ent.isInWater) return true;
    return isStandingOnFloor(bot);
}

async function closeWindowIfOpen(bot) {
    if (!bot?.currentWindow) return;
    try {
        await bot.closeWindow(bot.currentWindow);
    } catch {
        /* ignore */
    }
}

async function releaseForward(bot) {
    try {
        bot.setControlState('forward', false);
        bot.setControlState('jump', false);
    } catch {
        /* ignore */
    }
}

/** Держим W: повторный true no-op в patch, но страхует если кто-то сбросил. */
function holdForward(bot) {
    try {
        bot.setControlState('forward', true);
        bot.setControlState('jump', false);
        bot.setControlState('back', false);
        bot.setControlState('left', false);
        bot.setControlState('right', false);
        bot.refreshPlayerInput?.();
    } catch {
        /* ignore */
    }
}

/**
 * Look на ходу: force=true, но только целыми шагами мыши (GCD),
 * без snap на весь Δyaw — иначе FunAC видит «идеальную» мышь.
 */
function lookStepToward(bot, targetYaw, targetPitch, maxUnits = 14) {
    if (!bot?.entity) return;
    const curYaw = bot.entity.yaw;
    const curPitch = bot.entity.pitch;
    const dy = deltaYaw(targetYaw, curYaw);
    let nextYaw = curYaw;
    if (Math.abs(dy) > YAW_DEADBAND) {
        const units = Math.min(maxUnits, Math.floor(Math.abs(dy) / LOOK_GCD_STEP));
        if (units >= 1) {
            nextYaw = normalizeAngle(curYaw + Math.sign(dy) * units * LOOK_GCD_STEP);
        }
    }
    let nextPitch = curPitch;
    const dp = targetPitch - curPitch;
    if (Math.abs(dp) > LOOK_GCD_STEP) {
        const pu = Math.min(3, Math.floor(Math.abs(dp) / LOOK_GCD_STEP));
        if (pu >= 1) nextPitch = curPitch + Math.sign(dp) * pu * LOOK_GCD_STEP;
    }
    nextPitch = clampPitch(nextPitch);
    if (typeof bot.look === 'function') {
        bot.look(nextYaw, nextPitch, true).catch(() => {
            bot.entity.yaw = nextYaw;
            bot.entity.pitch = nextPitch;
        });
    } else {
        bot.entity.yaw = nextYaw;
        bot.entity.pitch = nextPitch;
    }
}

function pickDriftHeading(bot, preferYaw) {
    const sign = Math.random() < 0.5 ? -1 : 1;
    const prefer = preferYaw + sign * rnd(RETARGET_DELTA_MIN, RETARGET_DELTA_MAX);
    let h = pickSafeForwardYaw(bot, prefer);
    if (h == null) h = pickSafeForwardYaw(bot, bot.entity.yaw);
    if (h == null) return null;
    const dy = deltaYaw(h, bot.entity.yaw);
    if (Math.abs(dy) > 1.25) {
        h = bot.entity.yaw + Math.sign(dy) * rnd(0.5, 1.05);
        if (yawWouldFall(bot, h)) h = pickSafeForwardYaw(bot, bot.entity.yaw);
    }
    return h;
}

/** Yaw с достаточным коридором вперёд (не «край с walked=0»). */
function pickOpenForwardYaw(bot, preferYaw, minAhead) {
    const ent = bot?.entity;
    if (!ent) return null;
    const base = Number.isFinite(preferYaw) ? preferYaw : ent.yaw;
    const offsets = [
        0,
        0.2, -0.2, 0.45, -0.45, 0.8, -0.8,
        1.2, -1.2, 1.7, -1.7, 2.3, -2.3,
        Math.PI * 0.5, -Math.PI * 0.5, Math.PI, -Math.PI,
    ];
    let best = null;
    let bestAhead = 0;
    for (const off of offsets) {
        const y = base + off;
        if (yawWouldFall(bot, y)) continue;
        const ahead = maxSafeWalkDist(bot, y, 5);
        if (ahead >= minAhead) return y;
        if (ahead > bestAhead) {
            bestAhead = ahead;
            best = y;
        }
    }
    return bestAhead >= Math.min(1.0, minAhead * 0.55) ? best : null;
}

async function waitForFloor(bot, shouldAbort, maxMs = FLOOR_WAIT_MS) {
    const t0 = Date.now();
    while (Date.now() - t0 < maxMs) {
        if (typeof shouldAbort === 'function' && shouldAbort()) return false;
        if (!bot?.entity) return false;
        if (bot.entity.onGround || isStandingOnFloor(bot)) return true;
        await sleep(50);
    }
    return Boolean(bot?.entity?.onGround || isStandingOnFloor(bot));
}

/**
 * Anti-AFK / дневная прогулка: press W → идём + look на ходу → release W.
 * Как wire-запись пилота: редкий player_input, look GCD на ходу.
 *
 * По умолчанию длинная сессия (дневная). Для antiAfkIfNeeded — opts.short
 * или явные sessionMsMax ~3–5с.
 *
 * @param {import('mineflayer').Bot} bot
 * @param {Function|object} log
 * @param {Function|null} shouldAbort
 * @param {{
 *   short?: boolean,
 *   totalDistMin?: number,
 *   totalDistMax?: number,
 *   sessionMsMax?: number,
 *   zones?: { blocks?: (pos: object) => string|null },
 *   label?: string,
 * }} [opts]
 * @returns {Promise<number>} walked xz
 */
export async function antiAFKMoveForwardLook(bot, log = console.log, shouldAbort = null, opts = null) {
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? shouldAbort;
        opts = log.opts ?? log;
        log = log.log ?? console.log;
    }
    const options = opts && typeof opts === 'object' ? opts : {};
    const short = options.short === true;
    const label = typeof options.label === 'string'
        ? options.label
        : (short ? 'anti-AFK' : 'anti-AFK');
    const zones = options.zones || null;

    let distMin = Number.isFinite(options.totalDistMin) ? options.totalDistMin : TOTAL_DIST_MIN;
    let distMax = Number.isFinite(options.totalDistMax) ? options.totalDistMax : TOTAL_DIST_MAX;
    let sessionMs = Number.isFinite(options.sessionMsMax) ? options.sessionMsMax : SESSION_MS_MAX;
    if (short && !Number.isFinite(options.sessionMsMax)) {
        // сход с AFK / периодический осмотр: 3–5 с, не жрать sell-цикл
        sessionMs = rndInt(3_000, 5_000);
        distMin = Number.isFinite(options.totalDistMin) ? options.totalDistMin : 4;
        distMax = Number.isFinite(options.totalDistMax) ? options.totalDistMax : 22;
    }

    if (!bot?.entity || !bot.setControlState || typeof bot.look !== 'function') return 0;
    const logFn = typeof log === 'function' ? log : console.log;
    if (typeof shouldAbort === 'function' && shouldAbort()) return 0;

    await closeWindowIfOpen(bot);
    if (typeof shouldAbort === 'function' && shouldAbort()) return 0;

    if (!(await waitForFloor(bot, shouldAbort))) {
        logFn(`${label} look → стоп, нет пола`);
        return 0;
    }

    let targetYaw = pickOpenForwardYaw(bot, bot.entity.yaw + rnd(-0.35, 0.35), AHEAD_START_MIN);
    if (targetYaw == null) {
        targetYaw = pickOpenForwardYaw(bot, bot.entity.yaw, 1.2);
    }
    // После варпа чанки впереди ещё null → pit-guard режет всё. Ждём догрузку,
    // не жмём слепой emergency W (палит FunAC).
    if (targetYaw == null) {
        const waitCourseUntil = Date.now() + 2_500;
        logFn(`${label} look → нет курса, жду чанки…`);
        while (Date.now() < waitCourseUntil && targetYaw == null) {
            if (typeof shouldAbort === 'function' && shouldAbort()) return 0;
            await sleep(200);
            if (!(bot.entity?.onGround || isStandingOnFloor(bot))) break;
            targetYaw = pickOpenForwardYaw(bot, bot.entity.yaw + rnd(-0.35, 0.35), AHEAD_START_MIN)
                ?? pickOpenForwardYaw(bot, bot.entity.yaw, 1.2);
        }
    }
    if (targetYaw == null) {
        logFn(`${label} look → нет открытого курса, skip`);
        return 0;
    }

    // Сначала доворачиваем GCD-шагами (без snap), потом W
    let targetPitch = clampPitch(rnd(PITCH_IDLE_MIN, PITCH_IDLE_MAX));
    for (let i = 0; i < 8; i++) {
        const dy0 = deltaYaw(targetYaw, bot.entity.yaw);
        if (Math.abs(dy0) <= YAW_DEADBAND) break;
        lookStepToward(bot, targetYaw, targetPitch, rndInt(10, 16));
        await sleep(TICK_MS);
    }
    if (typeof shouldAbort === 'function' && shouldAbort()) return 0;

    const totalWant = rnd(Math.min(distMin, distMax), Math.max(distMin, distMax));
    const sessionEnd = Date.now() + Math.max(2_000, sessionMs);
    const origin0 = bot.entity.position.clone();
    let stuckN = 0;
    let pitSkips = 0;
    let retargets = 0;
    let airTicks = 0;

    let nextRetargetAt = Date.now() + rndInt(RETARGET_EVERY_MS_MIN, RETARGET_EVERY_MS_MAX);
    let nextPitCheck = Date.now() + PIT_CHECK_EVERY_MS;

    logFn(
        short
            ? `${label} → short W ~${(sessionMs / 1000).toFixed(1)}с / ≤${totalWant.toFixed(0)} блоков (look GCD)`
            : `${label} → continuous W ~${totalWant.toFixed(0)} блоков (look GCD на ходу)`,
    );
    clearWasd(bot);

    try {
        holdForward(bot);

        let stuckOrigin = bot.entity.position.clone();
        let stuckAt = Date.now();

        while (Date.now() < sessionEnd) {
            if (typeof shouldAbort === 'function' && shouldAbort()) break;

            if (zones?.blocks?.(bot.entity.position)) {
                logFn(`${label} look → зона «${zones.blocks(bot.entity.position)}», стоп`);
                break;
            }

            if (!canKeepWalking(bot)) {
                airTicks += 1;
                if (airTicks > 12) {
                    logFn(`${label} look → долго в воздухе, стоп`);
                    break;
                }
            } else {
                airTicks = 0;
            }

            const walked = horizDist(origin0, bot.entity.position);
            if (walked >= totalWant) break;

            const now = Date.now();

            holdForward(bot);

            if (now >= nextPitCheck) {
                nextPitCheck = now + PIT_CHECK_EVERY_MS;
                const ahead = maxSafeWalkDist(bot, bot.entity.yaw, 3.5);
                if (ahead < AHEAD_MIN_SAFE || yawWouldFall(bot, bot.entity.yaw)) {
                    pitSkips += 1;
                    const alt = pickOpenForwardYaw(
                        bot,
                        bot.entity.yaw + (Math.random() < 0.5 ? 1 : -1) * rnd(0.8, 1.8),
                        AHEAD_MIN_SAFE,
                    );
                    if (alt == null) {
                        if (walked < 1.0) {
                            logFn(`${label} look → край до хода, abort`);
                        } else {
                            logFn(`${label} look → край, некуда рулить`);
                        }
                        break;
                    }
                    targetYaw = alt;
                    nextRetargetAt = now + rndInt(700, 1_400);
                    retargets += 1;
                }
            } else if (now >= nextRetargetAt) {
                const h = pickDriftHeading(bot, targetYaw);
                if (h != null && maxSafeWalkDist(bot, h, 3.5) >= AHEAD_MIN_SAFE * 0.7) {
                    targetYaw = h;
                    if (Math.random() < 0.35) {
                        targetPitch = clampPitch(targetPitch + rnd(-0.05, 0.05));
                    }
                    retargets += 1;
                }
                nextRetargetAt = now + rndInt(RETARGET_EVERY_MS_MIN, RETARGET_EVERY_MS_MAX);
            }

            lookStepToward(bot, targetYaw, targetPitch, rndInt(8, 16));

            if (now - stuckAt >= STUCK_CHECK_MS) {
                const slid = horizDist(stuckOrigin, bot.entity.position);
                if (slid < STUCK_MOVE_MIN || horizSpeed(bot) < 0.035) {
                    stuckN += 1;
                    await releaseForward(bot);
                    const alt = pickOpenForwardYaw(
                        bot,
                        bot.entity.yaw + (Math.random() < 0.5 ? 1 : -1) * rnd(1.0, 2.2),
                        AHEAD_MIN_SAFE,
                    );
                    if (alt == null || stuckN >= 5) break;
                    targetYaw = alt;
                    for (let i = 0; i < 4; i++) {
                        lookStepToward(bot, targetYaw, targetPitch, rndInt(10, 16));
                        await sleep(TICK_MS);
                    }
                    holdForward(bot);
                    nextRetargetAt = now + 600;
                }
                stuckOrigin = bot.entity.position.clone();
                stuckAt = now;
            }

            await sleep(TICK_MS);
        }
    } finally {
        await releaseForward(bot);
        clearWasd(bot);
        try { bot.setControlState('jump', false); } catch { /* ignore */ }
        await awaitSettledOnFloor(bot, { shouldAbort, maxMs: 600 });
    }

    const walked = horizDist(origin0, bot.entity.position);
    logFn(
        `${label} look → конец (walked=${walked.toFixed(1)}/${totalWant.toFixed(1)}, `
        + `retargets=${retargets}, pit=${pitSkips}, stuck=${stuckN})`,
    );
    return walked;
}
