/**
 * Захват капчи → POST solver → цифры → при ошибке ретрай → /reg → /l
 */
import { waitForCaptcha, attachMapCache } from './capture.mjs';

export const CAPTCHA_CHAT_MARKER = 'BotFilter >> Введите номер с картинки в чат';
export const CAPTCHA_OK_MARKER = 'BotFilter >> Проверка пройдена, приятной игры';
const WRONG_MARK = 'капчу неправильно';
const MAX_ATTEMPTS = 3;

export function isCaptchaChat(text) {
    const s = String(text || '').toLowerCase();
    return (
        s.includes('введите номер с картинки') ||
        s.includes(CAPTCHA_CHAT_MARKER.toLowerCase())
    );
}

export function isCaptchaWrong(text) {
    return String(text || '').toLowerCase().includes(WRONG_MARK);
}

/** Успех: BotFilter >> Проверка пройдена, приятной игры */
export function isCaptchaOk(text) {
    const s = String(text || '').toLowerCase();
    return s.includes('проверка пройдена') || s.includes(CAPTCHA_OK_MARKER.toLowerCase());
}

function packetToText(data) {
    if (data?.plainMessage) return String(data.plainMessage);
    const raw = data?.formattedMessage ?? data?.content ?? data?.unsignedContent;
    if (raw == null) return '';
    if (typeof raw === 'string') {
        try {
            return flattenChat(JSON.parse(raw));
        } catch {
            return raw;
        }
    }
    return flattenChat(raw);
}

function flattenChat(raw) {
    if (raw == null) return '';
    if (typeof raw === 'string') return raw;
    if (typeof raw !== 'object') return String(raw);
    let out = raw.text ?? '';
    if (Array.isArray(raw.extra)) {
        for (const part of raw.extra) out += flattenChat(part);
    }
    if (raw.translate && Array.isArray(raw.with)) {
        for (const part of raw.with) out += flattenChat(part);
    }
    return out;
}

/**
 * Ждём ответ BotFilter после ввода цифр.
 * FunTime шлёт это в messagestr (не только systemChat).
 * 'wrong' | 'ok' («Проверка пройдена» или нет wrong за graceMs)
 */
function waitCaptchaVerdict(bot, { wrongMs = 12_000, okGraceMs = 2800 } = {}) {
    return new Promise((resolve) => {
        const client = bot._client;
        let settled = false;
        const finish = (v) => {
            if (settled) return;
            settled = true;
            clearTimeout(okTimer);
            clearTimeout(hardTimer);
            bot.off?.('messagestr', onMsg);
            client?.off('systemChat', onPkt);
            client?.off('playerChat', onPkt);
            resolve(v);
        };
        const onText = (text) => {
            if (!text) return;
            if (isCaptchaWrong(text)) finish('wrong');
            else if (isCaptchaOk(text)) finish('ok');
        };
        const onMsg = (msg) => onText(String(msg || ''));
        const onPkt = (data) => onText(packetToText(data));
        const okTimer = setTimeout(() => finish('ok'), okGraceMs);
        const hardTimer = setTimeout(() => finish('ok'), wrongMs);
        bot.on('messagestr', onMsg);
        client?.on('systemChat', onPkt);
        client?.on('playerChat', onPkt);
    });
}

export async function solveRemote(solverUrl, opts) {
    const base = String(solverUrl || '').replace(/\/$/, '');
    if (!base) throw new Error('solverUrl empty');
    const ctrl = new AbortController();
    const t = setTimeout(() => ctrl.abort(), opts.timeoutMs ?? 20_000);
    try {
        const res = await fetch(`${base}/api/solve`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                pngBase64: opts.pngBase64,
                username: opts.username || undefined,
                fingerprint: opts.fingerprint || undefined,
                dump: false,
            }),
            signal: ctrl.signal,
        });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
            throw new Error(`solve HTTP ${res.status}: ${body.detail || JSON.stringify(body)}`);
        }
        if (!body.pred || !/^\d{5}$/.test(body.pred)) {
            throw new Error(`bad pred ${JSON.stringify(body)}`);
        }
        // ok/low_conf оставляем вызывающему — pred для лога есть всегда
        return body;
    } finally {
        clearTimeout(t);
    }
}

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

/**
 * @param {import('mineflayer').Bot} bot
 * @param {{
 *   password: string,
 *   solverUrl: string,
 *   username?: string,
 *   quietMs?: number,
 *   maxWaitMs?: number,
 *   afterAnswerMs?: number,
 *   skipReg?: boolean,  // только /l (уже зарегистрированный акк, без /reg и привязки)
 *   afterRegMs?: number,
 *   maxAttempts?: number,
 *   log?: (...a:any[]) => void,
 * }} opts
 */
export async function handleCaptchaLogin(bot, opts) {
    const log = opts.log || ((...a) => console.log('[captcha]', ...a));
    const password = String(opts.password || '');
    if (!password) throw new Error('password required');
    const rawMax = opts.maxAttempts ?? MAX_ATTEMPTS;
    const infinite = rawMax === Infinity || rawMax === 0 || opts.infinite === true;
    const maxAttempts = infinite ? Infinity : rawMax;

    attachMapCache(bot);

    let prevFingerprint = null;
    let lastSolved = null;
    /** seq map-кэша на момент wrong — пакеты новой капчи часто приходят до attempt N+1 */
    let minMapSeq = 0;

    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
        log(
            infinite
                ? `attempt ${attempt}/∞ capture…`
                : `attempt ${attempt}/${maxAttempts} capture…`,
        );
        let captured;
        try {
            captured = await waitForCaptcha(bot, {
                quietMs: opts.quietMs ?? 900,
                maxWaitMs: opts.maxWaitMs ?? 20_000,
                prevFingerprint,
                // FunTime шлёт капчу снова; на ретрае ждём новые map (в т.ч. уже в кэше после wrong)
                requireFreshMaps: attempt > 1,
                minMapSeq,
                log,
            });
        } catch (e) {
            log(`capture error: ${e.message}${infinite ? ' — retry' : ''}`);
            if (!infinite) throw e;
            await sleep(1000);
            continue;
        }
        if (!captured?.png) {
            log(`captcha capture failed (attempt ${attempt})${infinite ? ' — retry' : ''}`);
            if (!infinite) {
                throw new Error(`captcha capture failed (attempt ${attempt})`);
            }
            await sleep(1000);
            continue;
        }
        prevFingerprint = captured.fingerprint;

        const pngBase64 = Buffer.isBuffer(captured.png)
            ? captured.png.toString('base64')
            : Buffer.from(captured.png).toString('base64');

        log('solving…', opts.solverUrl);
        let solved;
        try {
            solved = await solveRemote(opts.solverUrl, {
                pngBase64,
                username: opts.username || bot.username,
                fingerprint: captured.fingerprint,
            });
        } catch (e) {
            log(`solve error: ${e.message}${infinite ? ' — retry' : ''}`);
            if (!infinite) throw e;
            await sleep(1500);
            continue;
        }
        lastSolved = solved;
        const lowConf = solved.ok === false || solved.low_conf === true;
        log(
            'pred',
            solved.pred,
            'conf',
            solved.conf,
            `${solved.ms}ms`,
            lowConf ? 'LOW_CONF(still send)' : 'ok',
            solved.dump ? `dump=${solved.dump}` : '',
        );

        // seq до ответа: новая капча часто летит сразу после wrong, ещё до нашего retry
        const seqBeforeAnswer = bot._kapchaMapSeq || 0;
        // low_conf ≠ junk: FunTime всё равно жжёт попытку; pred хоть иногда проходит
        bot.chat(solved.pred);
        const verdict = await waitCaptchaVerdict(bot);
        if (verdict === 'wrong') {
            minMapSeq = seqBeforeAnswer;
            log(`wrong answer ${solved.pred}, retry… (mapSeq>${minMapSeq})`);
            await sleep(600);
            continue;
        }

        // нет «неправильно» — считаем ок
        await sleep(opts.afterAnswerMs ?? 400);
        if (!opts.skipReg) {
            bot.chat(`/reg ${password}`);
            await sleep(opts.afterRegMs ?? 600);
        }
        bot.chat(`/l ${password}`);
        return {
            pred: solved.pred,
            conf: solved.conf,
            ms: solved.ms,
            attempt,
            accepted: true,
        };
    }

    throw new Error(
        `captcha not accepted after ${maxAttempts} tries (last ${lastSolved?.pred})`,
    );
}

export { attachMapCache, waitForCaptcha };
