/**
 * Anti-AFK: только WASD, бурст ходьбы как до осмотра (~4с, клавиша ~0.7–1.2с).
 * Голову не крутим. Частоту задаёт вызывающий код (walkGap / antiAfkIfNeeded).
 * Клавиши в яму/обрыв не жмём (wasd-pit-guard) + тормоз инерции.
 */

import {
    awaitSettledOnFloor,
    clearWasd,
    isNearPitEdge,
    isStandingOnFloor,
    keyWouldFall,
    nextSafeWasdKey,
} from './wasd-pit-guard.mjs';

const WASD_KEYS = ['forward', 'left', 'back', 'right'];

const MOVE_BURST_MS_MIN = 3_500;
const MOVE_BURST_MS_MAX = 5_000;
const MOVE_KEY_HOLD_MS_MIN = 700;
const MOVE_KEY_HOLD_MS_MAX = 1_200;
const MOVE_KEY_PAUSE_MS_MIN = 80;
const MOVE_KEY_PAUSE_MS_MAX = 200;

/** @type {WeakMap<object, { keyIndex: number }>} */
const stateByBot = new WeakMap();

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function rnd(min, max) {
    return min + Math.random() * (max - min);
}

function rndInt(min, max) {
    return min + Math.floor(Math.random() * (max - min + 1));
}

function getState(bot) {
    let s = stateByBot.get(bot);
    if (!s) {
        s = { keyIndex: 0 };
        stateByBot.set(bot, s);
    }
    return s;
}

async function closeWindowIfOpen(bot) {
    if (!bot?.currentWindow) return;
    try {
        await bot.closeWindow(bot.currentWindow);
    } catch {
        /* ignore */
    }
}

/** Голову не крутим. */
export async function antiAFKdragMouse(_bot, _log = console.log, _shouldAbort = null) {
    return;
}

/** Бурст WASD ~3.5–5с (карусель клавиш), не один короткий тычок. */
export async function antiAFKMove(bot, log = console.log, shouldAbort = null) {
    if (!bot?.entity || !bot.setControlState) return;
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? null;
        log = log.log ?? console.log;
    }
    const logFn = typeof log === 'function' ? log : console.log;
    const st = getState(bot);

    if (typeof shouldAbort === 'function' && shouldAbort()) return;

    await closeWindowIfOpen(bot);
    if (typeof shouldAbort === 'function' && shouldAbort()) return;

    if (!isStandingOnFloor(bot)) {
        logFn('anti-AFK → стоп, нет пола под ногами (яма/полёт)');
        return;
    }
    if (isNearPitEdge(bot)) {
        logFn('anti-AFK → стоп, у края ямы (не жмём WASD)');
        await awaitSettledOnFloor(bot, { shouldAbort });
        return;
    }

    const burstMs = rndInt(MOVE_BURST_MS_MIN, MOVE_BURST_MS_MAX);
    const endAt = Date.now() + burstMs;
    logFn(`anti-AFK ходьба ~${(burstMs / 1000).toFixed(1)}с (бурст WASD)`);

    let pitSkips = 0;
    try {
        while (Date.now() < endAt) {
            if (typeof shouldAbort === 'function' && shouldAbort()) break;
            if (!isStandingOnFloor(bot)) {
                logFn('anti-AFK → стоп mid-burst, пол пропал');
                clearWasd(bot);
                break;
            }
            if (isNearPitEdge(bot)) {
                pitSkips++;
                logFn('anti-AFK → у края, стоп бурста');
                await awaitSettledOnFloor(bot, { shouldAbort });
                break;
            }

            const key = nextSafeWasdKey(bot, st, WASD_KEYS);
            if (!key) {
                pitSkips++;
                logFn('anti-AFK → все направления в яму, стоп');
                await awaitSettledOnFloor(bot, { shouldAbort });
                break;
            }
            const holdMs = rndInt(MOVE_KEY_HOLD_MS_MIN, MOVE_KEY_HOLD_MS_MAX);
            logFn(`anti-AFK клавиша ${key.toUpperCase()} ${holdMs}мс`);

            let hitEdge = false;
            try {
                bot.setControlState(key, true);
                const holdUntil = Date.now() + holdMs;
                while (Date.now() < holdUntil) {
                    if (typeof shouldAbort === 'function' && shouldAbort()) break;
                    if (Date.now() >= endAt) break;
                    if (keyWouldFall(bot, key) || !isStandingOnFloor(bot) || isNearPitEdge(bot)) {
                        pitSkips++;
                        hitEdge = true;
                        logFn(`anti-AFK → край/яма на ${key}, отпускаю`);
                        break;
                    }
                    await sleep(Math.min(40, holdUntil - Date.now()));
                }
            } finally {
                try {
                    bot.setControlState(key, false);
                } catch {
                    /* ignore */
                }
            }

            if (hitEdge) {
                await awaitSettledOnFloor(bot, { shouldAbort });
                if (!isStandingOnFloor(bot) || isNearPitEdge(bot)) {
                    logFn('anti-AFK → после тормоза всё ещё край/полёт, стоп');
                    break;
                }
            }

            if (typeof shouldAbort === 'function' && shouldAbort()) break;
            if (Date.now() >= endAt) break;
            await sleep(rnd(MOVE_KEY_PAUSE_MS_MIN, MOVE_KEY_PAUSE_MS_MAX));
        }
    } finally {
        clearWasd(bot);
    }
    if (pitSkips) logFn(`anti-AFK → pit-guard срабатываний: ${pitSkips}`);
}

/**
 * Полный тик: только WASD-бурст. opts.force игнорируется.
 */
export async function runAntiAfkMotion(bot, log = console.log, shouldAbort = null, _opts = null) {
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? null;
        log = log.log ?? console.log;
    }

    await antiAFKMove(bot, log, shouldAbort);
}

/** Совместимость: lookAroundSpin ≡ runAntiAfkMotion. */
export async function lookAroundSpin(bot, log = console.log, shouldAbort = null, opts = null) {
    return runAntiAfkMotion(bot, log, shouldAbort, opts);
}

/**
 * @param {import('mineflayer').Bot} bot
 * @param {{ afk: boolean }} state
 */
export async function antiAfkIfNeeded(bot, state, log = console.log, shouldAbort = null, closeWindow = null) {
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? null;
        closeWindow = log.closeWindow ?? null;
        log = log.log ?? console.log;
    }

    if (!state.afk) return;
    if (typeof shouldAbort === 'function' && shouldAbort()) return;

    const logFn = typeof log === 'function' ? log : console.log;
    logFn('AFK → ходьба');

    if (typeof closeWindow === 'function') {
        await closeWindow();
    } else {
        await closeWindowIfOpen(bot);
    }

    await runAntiAfkMotion(bot, logFn, shouldAbort);
    state.afk = false;
    logFn('AFK снят');
}

/** Чат-маркер FunTime → AFK. */
export function noteAfkChat(text, state) {
    if (!text || !state) return false;
    if (text.includes('Данная команда недоступна в режиме AFK')) {
        state.afk = true;
        return true;
    }
    return false;
}

/** Следующий интервал walkTime: 50–60 с. */
export function nextWalkGapMs() {
    return 50_000 + Math.floor(Math.random() * 10_001);
}
