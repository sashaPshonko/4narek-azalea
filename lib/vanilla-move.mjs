/**
 * Anti-AFK: WASD-бурст по умолчанию; на 502/504 / lookSteer — forward+мышь
 * (см. afk-forward-look.mjs).
 * Протокол 1.21 как ваниль: полный player_input при смене клавиш;
 * position/look → потом tick_end; hasHorizontalCollision из prismarine;
 * sprint не используем. Единый patch для 4narek-old / 4NAREK / roles.
 * Ямы: wasd-pit-guard + тормоз инерции; у края бурст не стартуем.
 */

import {
    awaitSettledOnFloor,
    clearWasd,
    isNearPitEdge,
    isStandingOnFloor,
    keyWouldFall,
    nextSafeWasdKey,
} from './wasd-pit-guard.mjs';
import { antiAFKMoveForwardLook } from './afk-forward-look.mjs';

const WASD_KEYS = ['forward', 'left', 'back', 'right'];

/** Как TIMING.MOVE_BURST / MOVE_KEY_* до осмотра. */
const MOVE_BURST_MS_MIN = 3_500;
const MOVE_BURST_MS_MAX = 5_000;
const MOVE_KEY_HOLD_MS_MIN = 700;
const MOVE_KEY_HOLD_MS_MAX = 1_200;
const MOVE_KEY_PAUSE_MS_MIN = 80;
const MOVE_KEY_PAUSE_MS_MAX = 200;

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function rndInt(min, max) {
    return min + Math.floor(Math.random() * (max - min + 1));
}

async function closeWindowIfOpen(bot) {
    if (!bot?.currentWindow) return;
    try {
        await bot.closeWindow(bot.currentWindow);
    } catch {
        /* ignore */
    }
}

/** Голову не крутим. Оставлено no-op, чтобы старые вызовы не слали look. */
export async function antiAFKdragMouse(_bot, _log = console.log, _shouldAbort = null) {
    return;
}

/**
 * Один короткий WASD-ход (~0.7–1.2с) — fallback после short look-steer.
 */
export async function antiAFKMoveShort(bot, log = console.log, shouldAbort = null) {
    if (!bot?.entity || !bot.setControlState) return;
    const logFn = typeof log === 'function' ? log : console.log;
    if (typeof shouldAbort === 'function' && shouldAbort()) return;

    await closeWindowIfOpen(bot);
    if (typeof shouldAbort === 'function' && shouldAbort()) return;
    if (!isStandingOnFloor(bot) || isNearPitEdge(bot)) return;

    const st = { keyIndex: rndInt(0, WASD_KEYS.length - 1) };
    const key = nextSafeWasdKey(bot, st, WASD_KEYS);
    if (!key) return;
    const holdMs = rndInt(MOVE_KEY_HOLD_MS_MIN, MOVE_KEY_HOLD_MS_MAX);
    logFn(`anti-AFK mini ${key.toUpperCase()} ${holdMs}мс`);
    try {
        clearWasd(bot);
        bot.setControlState(key, true);
        bot.refreshPlayerInput?.();
        const end = Date.now() + holdMs;
        while (Date.now() < end) {
            if (typeof shouldAbort === 'function' && shouldAbort()) break;
            if (keyWouldFall(bot, key) || isNearPitEdge(bot)) break;
            await sleep(40);
        }
    } finally {
        clearWasd(bot);
        await awaitSettledOnFloor(bot, { shouldAbort, maxMs: 400 });
    }
}

/**
 * Бурст WASD ~3.5–5с: безопасные клавиши (не в яму), удержание ~0.7–1.2с.
 */
export async function antiAFKMove(bot, log = console.log, shouldAbort = null) {
    if (!bot?.entity || !bot.setControlState) return;
    const logFn = typeof log === 'function' ? log : console.log;
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
    const st = { keyIndex: rndInt(0, WASD_KEYS.length - 1) };
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
            await sleep(rndInt(MOVE_KEY_PAUSE_MS_MIN, MOVE_KEY_PAUSE_MS_MAX));
        }
    } finally {
        clearWasd(bot);
    }
    if (pitSkips) logFn(`anti-AFK → pit-guard срабатываний: ${pitSkips}`);
}

/**
 * @param {import('mineflayer').Bot} bot
 * @param {Function|object} log
 * @param {Function|null} shouldAbort
 * @param {{ lookSteer?: boolean, anarchy?: number }} [_opts]
 *   lookSteer / anarchy===502|504 → forward+мышь; иначе классический WASD-бурст.
 * @returns {Promise<number>} пройденный xz (0 если не сдвинулись)
 */
export async function runAntiAfkMotion(bot, log = console.log, shouldAbort = null, _opts = null) {
    let opts = _opts;
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? null;
        opts = log.opts ?? log;
        log = log.log ?? console.log;
    }
    const an = Number(opts?.anarchy);
    const lookSteer = opts?.lookSteer === true
        || an === 502
        || an === 504
        || process.env.AFK_LOOK_STEER === '1';
    if (lookSteer) {
        // только short W+look (как дневная, короче). Без emergency/full WASD —
        // FunAC 4.3 цеплял аварийное поведение после раннего warp.
        return antiAFKMoveForwardLook(bot, log, shouldAbort, {
            short: true,
            label: 'anti-AFK',
        });
    }
    const p0 = bot?.entity?.position;
    const x0 = p0?.x;
    const z0 = p0?.z;
    await antiAFKMove(bot, log, shouldAbort);
    const p1 = bot?.entity?.position;
    if (
        Number.isFinite(x0) && Number.isFinite(z0)
        && p1 && Number.isFinite(p1.x) && Number.isFinite(p1.z)
    ) {
        return Math.hypot(p1.x - x0, p1.z - z0);
    }
    return 0;
}

export async function lookAroundSpin(bot, log = console.log, shouldAbort = null, opts = null) {
    return runAntiAfkMotion(bot, log, shouldAbort, opts);
}

/** Снимаем локальный AFK только если реально сдвинулись (сервер иначе остаётся в AFK). */
const AFK_CLEAR_MIN_WALK = 1.0;

export async function antiAfkIfNeeded(bot, state, log = console.log, shouldAbort = null, closeWindow = null) {
    if (log && typeof log === 'object' && !Array.isArray(log)) {
        shouldAbort = log.shouldAbort ?? null;
        closeWindow = log.closeWindow ?? null;
        log = log.log ?? console.log;
    }
    if (!state.afk) return 0;
    if (typeof shouldAbort === 'function' && shouldAbort()) return 0;
    const logFn = typeof log === 'function' ? log : console.log;
    logFn('AFK → ходьба');
    if (typeof closeWindow === 'function') await closeWindow();
    else await closeWindowIfOpen(bot);
    const walked = await runAntiAfkMotion(bot, logFn, shouldAbort);
    if (walked >= AFK_CLEAR_MIN_WALK) {
        state.afk = false;
        logFn(`AFK снят (walked=${walked.toFixed(1)})`);
    } else {
        logFn(`AFK не снят — walked=${walked.toFixed(1)} < ${AFK_CLEAR_MIN_WALK}`);
    }
    return walked;
}

export function noteAfkChat(text, state) {
    if (!text || !state) return false;
    if (text.includes('Данная команда недоступна в режиме AFK')) {
        state.afk = true;
        return true;
    }
    return false;
}

export function nextWalkGapMs() {
    return 50_000 + Math.floor(Math.random() * 10_001);
}

/** mineflayer control → protocol 1.21 Input flags (полный набор, как ваниль). */
const CONTROL_TO_INPUT = {
    forward: 'forward',
    back: 'backward',
    left: 'left',
    right: 'right',
    jump: 'jump',
    sneak: 'shift',
    sprint: 'sprint',
};

const INPUT_FLAG_TO_CONTROL = {
    forward: 'forward',
    backward: 'back',
    left: 'left',
    right: 'right',
    jump: 'jump',
    shift: 'sneak',
    sprint: 'sprint',
};

const MOVE_PACKET_NAMES = new Set(['position', 'look', 'position_look', 'flying']);

/** Sprint entity_action ids (string mapper + legacy). */
function isSprintStartAction(params) {
    const aid = params?.actionId;
    return aid === 'start_sprinting' || aid === 3;
}

/**
 * 1.21 как ваниль (единый patch для всех entrypoints):
 * - player_input только при смене, всегда полный bitset (7 флагов)
 * - sprint не используем: ignore true + глотаем start_sprinting
 * - глотаем урезанный {shift} из mineflayer physics
 * - по умолчанию НЕ трогаем position/look (fround+collision ломали Y→70 и /ah)
 * - tick_end: baseline — после player_input; ванильнее — каждый physicsTick (ClientTickEnd)
 *
 * Флаги (локальные A/B в 4NAREK/):
 * @param {import('mineflayer').Bot} bot
 * @param {{
 *   rewriteMovePackets?: boolean,
 *   tickEndOnMove?: boolean,
 *   tickEndEveryTick?: boolean,
 *   froundMovePos?: boolean,
 *   collisionFlags?: boolean,
 *   horizMode?: '' | 'forceFalse' | 'forceTrue',
 * }} [opts]
 *   rewriteMovePackets: true = tickEndOnMove+froundMovePos+collisionFlags (старый полный).
 *   tickEndEveryTick: 1× tick_end на physicsTick (как ClientTickEndC2SPacket).
 *   horizMode: forceFalse/forceTrue — для A/B причины E5 FAIL.
 *   Прод: input-only + tickEndEveryTick (E14 PASS 16.09).
 */
export function patchWalking(bot, opts = {}) {
    if (!bot || bot._vanillaMovePatched) return;
    bot._vanillaMovePatched = true;
    bot._walk121Patched = true;

    const full = opts.rewriteMovePackets === true;
    const flags = {
        tickEndOnMove: opts.tickEndOnMove === true || full,
        tickEndEveryTick: opts.tickEndEveryTick === true,
        froundMovePos: opts.froundMovePos === true || full,
        collisionFlags: opts.collisionFlags === true || full,
        horizMode: opts.horizMode === 'forceFalse' || opts.horizMode === 'forceTrue'
            ? opts.horizMode
            : '',
    };

    if (typeof bot.setControlState === 'function' && bot._client) {
        applyWalkPatch(bot, flags);
    } else {
        bot.once('inject_allowed', () => setTimeout(
            () => applyWalkPatch(bot, flags),
            0,
        ));
    }
}

/** Alias: старые импорты walk-121. */
export const patchWalking121 = patchWalking;

function fullInputsFrom(controlState) {
    const inputs = {};
    for (const [control, flag] of Object.entries(CONTROL_TO_INPUT)) {
        // sprint всегда false — не шлём в input
        inputs[flag] = control === 'sprint' ? false : !!controlState[control];
    }
    return inputs;
}

function applyWalkPatch(bot, {
    tickEndOnMove = false,
    tickEndEveryTick = false,
    froundMovePos = false,
    collisionFlags = false,
    horizMode = '',
} = {}) {
    if (typeof bot.setControlState !== 'function') return;
    if (bot._vanillaMoveControls) return;

    const controlState = {
        forward: false,
        back: false,
        left: false,
        right: false,
        jump: false,
        sprint: false,
        sneak: false,
    };
    bot._vanillaMoveControls = controlState;
    bot._walk121Controls = controlState;
    const origSetControlState = bot.setControlState.bind(bot);
    const client = bot._client;
    if (!client || typeof client.write !== 'function') return;

    const rawWrite = client.write.bind(client);
    let suppressPlayerInput = false;
    let tickEndQueued = false;
    const stats = {
        player_input: 0,
        tick_end: 0,
        position: 0,
        look: 0,
        position_look: 0,
        flying: 0,
        entity_action_drop: 0,
        horiz_true: 0,
        horiz_false: 0,
        physics_ticks: 0,
    };
    bot._vanillaMoveStats = stats;

    /** Исходящий пакет на FunTime — для wire-записи пилота / сравнения с ванилью. */
    const sendOut = (name, params) => {
        try {
            bot.emit('vanillaOut', name, params);
        } catch {
            /* ignore */
        }
        return rawWrite(name, params);
    };

    const queueTickEnd = () => {
        if (tickEndQueued) return;
        tickEndQueued = true;
        queueMicrotask(() => {
            tickEndQueued = false;
            if (client.state !== 'play') return;
            try {
                sendOut('tick_end', {});
                stats.tick_end += 1;
            } catch {
                /* ignore */
            }
        });
    };

    /** После input: tick_end сразу, только если нет client-clock (иначе clock закроет тик). */
    const tickEndAfterInput = () => {
        if (live.tickEndEveryTick) return;
        queueTickEnd();
    };

    const writeInputs = () => {
        if (client.state !== 'play') return;
        try {
            sendOut('player_input', { inputs: fullInputsFrom(controlState) });
            stats.player_input += 1;
            tickEndAfterInput();
        } catch {
            /* ignore */
        }
    };

    const live = {
        tickEndOnMove: !!tickEndOnMove,
        tickEndEveryTick: !!tickEndEveryTick,
        froundMovePos: !!froundMovePos,
        collisionFlags: !!collisionFlags,
        horizMode: horizMode || '',
    };
    bot._vanillaMoveFlags = live;
    bot.setVanillaMoveFlags = (partial = {}) => {
        if (partial.tickEndOnMove !== undefined) live.tickEndOnMove = !!partial.tickEndOnMove;
        if (partial.tickEndEveryTick !== undefined) live.tickEndEveryTick = !!partial.tickEndEveryTick;
        if (partial.froundMovePos !== undefined) live.froundMovePos = !!partial.froundMovePos;
        if (partial.collisionFlags !== undefined) live.collisionFlags = !!partial.collisionFlags;
        if (partial.horizMode !== undefined) live.horizMode = partial.horizMode || '';
        bot._vanillaMoveRewritePos = live.froundMovePos;
        return { ...live };
    };
    bot.resetVanillaMoveStats = () => {
        for (const k of Object.keys(stats)) stats[k] = 0;
    };

    let lastPhysicsTickAt = 0;
    bot.on('physicsTick', () => {
        stats.physics_ticks += 1;
        lastPhysicsTickAt = Date.now();
        // tick_end больше НЕ отсюда: mineflayer глотает physicsTick когда chunk
        // под ногами null → FunTime без ClientTickEnd глушит /ah|/balance.
        // Ваниль шлёт tick_end каждые 50мс всегда — см. clientTick ниже.
    });

    // ClientTickEnd как у TLauncher: 20 TPS в play, даже если physics встал
    // (chunk unload / blockAt null / GUI). Иначе сессия «play+ground», а команд нет.
    // ClientTickEnd как у TLauncher: 20 TPS в play.
    // Position-heartbeat при stall — тихо (раз в 30с warn), не спамить.
    if (live.tickEndEveryTick) {
        let stallPosWarnedAt = 0;
        let lastStallPosAt = 0;
        const clientTick = setInterval(() => {
            if (!live.tickEndEveryTick) return;
            if (client.state !== 'play' || client.ended) return;
            queueTickEnd();

            const stalledMs = lastPhysicsTickAt ? Date.now() - lastPhysicsTickAt : 9999;
            if (stalledMs < 500) return;
            if (Date.now() - lastStallPosAt < 1000) return;
            const ent = bot.entity;
            const pos = ent?.position;
            if (!pos || !Number.isFinite(pos.x)) return;
            try {
                const onGround = Boolean(ent.onGround);
                sendOut('position', {
                    x: pos.x,
                    y: pos.y,
                    z: pos.z,
                    onGround,
                    flags: { onGround, hasHorizontalCollision: false },
                });
                lastStallPosAt = Date.now();
                stats.position += 1;
                if (stalledMs > 5_000 && Date.now() - stallPosWarnedAt > 30_000) {
                    stallPosWarnedAt = Date.now();
                    try {
                        console.warn(
                            `[${bot.username || 'bot'}] ⚡ physics stall ${Math.round(stalledMs / 1000)}с (нет чанка?)`,
                        );
                    } catch { /* ignore */ }
                }
            } catch {
                /* ignore */
            }
        }, 50);
        clientTick.unref?.();
        const stopTick = () => clearInterval(clientTick);
        client.on('end', stopTick);
        bot.once?.('end', stopTick);
    }

    const fixMoveParams = (params) => {
        if (!params || typeof params !== 'object') return params;
        // Не мутируем lastSent mineflayer — всегда новый объект на провод
        const out = { ...params };
        if (live.collisionFlags || live.horizMode) {
            const onGround = !!(params.onGround ?? params.flags?.onGround);
            let horiz = !!(bot.entity?.isCollidedHorizontally);
            if (live.horizMode === 'forceFalse') horiz = false;
            if (live.horizMode === 'forceTrue') horiz = true;
            out.onGround = onGround;
            out.flags = { onGround, hasHorizontalCollision: horiz };
            stats.horiz_true += horiz ? 1 : 0;
            stats.horiz_false += horiz ? 0 : 1;
        }
        if (live.froundMovePos) {
            if (Number.isFinite(out.x)) out.x = Math.fround(out.x);
            if (Number.isFinite(out.y)) out.y = Math.fround(out.y);
            if (Number.isFinite(out.z)) out.z = Math.fround(out.z);
            if (Number.isFinite(out.yaw)) out.yaw = Math.fround(out.yaw);
            if (Number.isFinite(out.pitch)) out.pitch = Math.fround(out.pitch);
        }
        return out;
    };

    const shouldTouchMove = () =>
        live.tickEndOnMove || live.froundMovePos || live.collisionFlags || Boolean(live.horizMode);

    client.write = (name, params) => {
        if (name === 'entity_action' && isSprintStartAction(params)) {
            stats.entity_action_drop += 1;
            return;
        }
        if (name === 'player_input') {
            if (suppressPlayerInput) return;
            if (client.state !== 'play') return;
            const partial = params?.inputs;
            if (partial && typeof partial === 'object') {
                for (const [flag, control] of Object.entries(INPUT_FLAG_TO_CONTROL)) {
                    if (!Object.prototype.hasOwnProperty.call(partial, flag)) continue;
                    if (control === 'sprint') {
                        controlState.sprint = false;
                        continue;
                    }
                    controlState[control] = !!partial[flag];
                }
            }
            const out = sendOut('player_input', { inputs: fullInputsFrom(controlState) });
            stats.player_input += 1;
            tickEndAfterInput();
            return out;
        }
        if (MOVE_PACKET_NAMES.has(name) && shouldTouchMove()) {
            if (Object.prototype.hasOwnProperty.call(stats, name)) stats[name] += 1;
            const out = sendOut(name, fixMoveParams(params));
            // per-tick режим сам закрывает тик; иначе — после move как E1
            if (live.tickEndOnMove && !live.tickEndEveryTick) queueTickEnd();
            return out;
        }
        return sendOut(name, params);
    };

    bot.setControlState = function setControlStateVanilla(control, state) {
        if (!(control in controlState) || typeof state !== 'boolean') {
            return origSetControlState(control, state);
        }
        if (control === 'sprint') {
            if (state) return;
            if (!controlState.sprint) return;
            controlState.sprint = false;
            suppressPlayerInput = true;
            try {
                origSetControlState('sprint', false);
            } finally {
                suppressPlayerInput = false;
            }
            writeInputs();
            return;
        }
        if (controlState[control] === state) return;
        controlState[control] = state;

        suppressPlayerInput = true;
        let result;
        try {
            result = origSetControlState(control, state);
        } finally {
            suppressPlayerInput = false;
        }
        writeInputs();
        return result;
    };

    bot.refreshPlayerInput = writeInputs;
    bot._vanillaMoveRewritePos = live.froundMovePos;
}
