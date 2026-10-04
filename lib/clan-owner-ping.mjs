/**
 * Короткий заход владельца клана: жив / забанен.
 * Прокси + configuration-transfer — как в clan-setup (иначе FunTime рвёт до spawn).
 */
import mineflayer from 'mineflayer';
import { SocksClient } from 'socks';
import { SocksProxyAgent } from 'socks-proxy-agent';
import { handleCaptchaLogin, attachMapCache, isCaptchaChat } from './captcha/solve-flow.mjs';
import { acceptResourcePackVanilla } from './vanilla-resource-pack.mjs';

const SOLVER_URL = process.env.CAPTCHA_SOLVER_URL || 'http://127.0.0.1:8799';
const GO_HTTP = process.env.GO_HTTP_URL
    || (process.env.LOCAL_MODE === '1' || process.env.LOCAL_MODE === 'true'
        ? 'http://127.0.0.1:8080'
        : 'http://212.8.229.76:8080');

const CONFIG_BLOCKED = new Set([
    'position', 'look', 'position_look', 'flying',
    'chat', 'chat_command', 'chat_command_signed', 'chat_message',
    'window_click', 'close_window',
    'arm_animation', 'entity_action',
    'held_item_slot', 'set_creative_slot',
]);

export function isBanChatText(text) {
    // Только чат с «ВЫ ЗАБАНЕНЫ!» — не кик-причина.
    const s = String(text || '').toLowerCase().replace(/ё/g, 'е');
    return s.includes('вы забанены');
}

/** В reason — сам бан-бокс, без «Вы были кикнуты при подключении…». */
export function extractBanReason(text) {
    const raw = String(text || '').replace(/\r/g, '');
    const box = raw.match(/╔[\s\S]*?вы\s*забанены[\s\S]*?╚[^╚\n]*╝/i);
    if (box) return box[0].trim().slice(0, 2000);
    const from = raw.search(/вы\s*забанены/i);
    if (from < 0) return '';
    return raw.slice(from).trim().slice(0, 2000);
}

/** Ник уже в игре с другого клиента — для пинга это «жив», не ошибка. */
export function isNickAlreadyOnlineText(text) {
    const s = String(text || '').toLowerCase().replace(/ё/g, 'е');
    return s.includes('ником уже онлайн')
        || s.includes('таким-же ником')
        || s.includes('таким-же именем играет');
}

function packetToText(data) {
    if (data == null) return '';
    if (typeof data === 'string') return data;
    if (data.plainMessage) return String(data.plainMessage);
    try {
        return JSON.stringify(data.formattedMessage ?? data.content ?? data.unsignedContent ?? data);
    } catch {
        return String(data);
    }
}

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function requestFunauthBindHttp(nick, password, anarchy) {
    if (!nick || !password) return;
    fetch(`${GO_HTTP}/api/funauth/bind`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ nick, password, anarchy }),
    }).catch(() => {});
}

function requestFunauthTwoFaHttp(nick, anarchy) {
    if (!nick) return;
    fetch(`${GO_HTTP}/api/funauth/2fa`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ nick, anarchy }),
    }).catch(() => {});
}

function requestFunauthVerifiedHttp(nick, anarchy) {
    if (!nick) return;
    fetch(`${GO_HTTP}/api/funauth/verified`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ nick, anarchy }),
    }).catch(() => {});
}

function flattenKickReason(reason) {
    if (reason == null) return '';
    if (typeof reason === 'string') return reason;
    try {
        return JSON.stringify(reason);
    } catch {
        return String(reason);
    }
}

function ensureRegistryDimensionStub(bot) {
    if (Array.isArray(bot.registry.dimensionsArray) && bot.registry.dimensionsArray.length > 0) {
        return;
    }
    const fallback = { name: 'minecraft:overworld', minY: -64, height: 384 };
    bot.registry.dimensionsArray = Array.from({ length: 16 }, () => fallback);
    bot.registry.dimensionsByName ??= { overworld: fallback };
}

function setupConfigurationTransferFix(bot, log) {
    const client = bot._client;
    if (!client) return;

    const origLoad = bot.registry.loadDimensionCodec.bind(bot.registry);
    bot.registry.loadDimensionCodec = (codec) => {
        try {
            origLoad(codec);
        } catch {
            ensureRegistryDimensionStub(bot);
        }
    };

    client.prependListener('login', () => ensureRegistryDimensionStub(bot));
    client.prependListener('respawn', () => ensureRegistryDimensionStub(bot));

    let blockSelectKnownPacksWrite = false;
    const origWrite = client.write.bind(client);
    client.write = (name, params) => {
        if (client.state === 'configuration' && CONFIG_BLOCKED.has(name)) return;
        if (blockSelectKnownPacksWrite && name === 'select_known_packs') return;
        return origWrite(name, params);
    };

    client.on('start_configuration', () => {
        blockSelectKnownPacksWrite = false;
        bot.physicsEnabled = false;
        log('transfer → configuration');
    });
    client.on('finish_configuration', () => {
        blockSelectKnownPacksWrite = false;
        bot.physicsEnabled = true;
        log('transfer → configuration ok');
    });
    client.on('cookie_request', (data) => {
        if (client.state !== 'configuration') return;
        origWrite('cookie_response', { key: data.cookie });
    });
    client.prependListener('add_resource_pack', (data) => {
        if (client.state !== 'configuration') return;
        void acceptResourcePackVanilla(origWrite, {
            uuid: data.uuid,
            url: data.url,
            log: (m) => log(m),
            shouldAbort: () => !bot?._client || bot._client.ended,
        });
    });
    client.prependListener('select_known_packs', (data) => {
        if (client.state !== 'configuration') return;
        const packs = (data.packs ?? []).map((p) => ({
            namespace: p.namespace,
            id: p.id,
            version: p.version,
        }));
        blockSelectKnownPacksWrite = true;
        try {
            origWrite('select_known_packs', { packs });
        } finally {
            blockSelectKnownPacksWrite = false;
        }
    });
}

/** Как в clan-setup: agent object + destination на FunTime. */
function buildProxyConnect(proxyString) {
    const url = new URL(proxyString);
    const proxyHost = url.hostname;
    const proxyPort = Number(url.port);
    const proxyUsername = url.username ? decodeURIComponent(url.username) : undefined;
    const proxyPassword = url.password ? decodeURIComponent(url.password) : undefined;
    const agent = new SocksProxyAgent({
        protocol: 'socks5:',
        host: proxyHost,
        port: proxyPort,
        username: proxyUsername,
        password: proxyPassword,
    });
    const connect = (client) => {
        SocksClient.createConnection(
            {
                proxy: {
                    host: proxyHost,
                    port: proxyPort,
                    type: 5,
                    userId: proxyUsername,
                    password: proxyPassword,
                },
                command: 'connect',
                destination: { host: 'mc.funtime.su', port: 25565 },
                timeout: 20_000,
            },
            (err, info) => {
                if (err) {
                    client.emit('error', err);
                    return;
                }
                client.setSocket(info.socket);
                client.emit('connect');
            },
        );
    };
    return { agent, connect, label: `${proxyHost}:${proxyPort}` };
}

/**
 * @param {{ username: string, password: string, proxyString: string, anarchy?: string|number, log?: Function }} opts
 * @returns {Promise<{ status: 'ok'|'banned'|'error', reason?: string }>}
 */
export async function pingClanOwner({ username, password, proxyString, anarchy, log = console.log }) {
    if (!username || !password || !proxyString) {
        return { status: 'error', reason: 'нет username/password/proxy' };
    }
    const an = String(anarchy ?? '').replace(/\D/g, '').slice(0, 3);

    const proxy = buildProxyConnect(proxyString);
    let settled = false;
    let bot = null;
    let lastKick = '';
    let joinedAnarchy = false;
    let captchaBusy = false;
    let funauthBindRequired = false;
    let funauthVerifyTimer = null;

    const cancelFunauthVerify = () => {
        if (funauthVerifyTimer) {
            clearTimeout(funauthVerifyTimer);
            funauthVerifyTimer = null;
        }
    };

    const finish = (result) => {
        if (settled) return result;
        settled = true;
        try {
            bot?.removeAllListeners?.();
            bot?.quit?.();
        } catch { /* ignore */ }
        return result;
    };

    return new Promise((resolve) => {
        const done = (result) => resolve(finish(result));
        const timer = setTimeout(() => {
            log(`[clan-owner-ping] ${username} timeout`);
            done({ status: 'error', reason: 'timeout' });
        }, 120_000);

        try {
            bot = mineflayer.createBot({
                username,
                password,
                host: 'mc.funtime.su',
                port: 25565,
                version: '1.21.11',
                physicsEnabled: false,
                hideErrors: true,
                logErrors: false,
                agent: proxy.agent,
                connect: proxy.connect,
                checkTimeoutInterval: 60_000,
            });
        } catch (e) {
            clearTimeout(timer);
            done({ status: 'error', reason: e.message });
            return;
        }

        const plog = (...a) => log(`[clan-owner-ping] ${username} ${a.join(' ')}`);
        const botAlive = () => !settled && bot?._client?.ended !== true;
        attachMapCache(bot);
        setupConfigurationTransferFix(bot, plog);

        const onChatText = (raw) => {
            const text = String(raw || '');
            if (!text || settled) return;

            // бан = только это сообщение в чате, не kicked
            if (isBanChatText(text)) {
                const reason = extractBanReason(text) || 'ВЫ ЗАБАНЕНЫ!';
                plog(`ban chat: ${reason.slice(0, 220)}`);
                clearTimeout(timer);
                done({ status: 'banned', reason });
                return;
            }

            if (isCaptchaChat(text)) {
                if (captchaBusy || !botAlive()) return;
                captchaBusy = true;
                plog('капча — до 3 попыток (не ∞)');
                void handleCaptchaLogin(bot, {
                    password,
                    solverUrl: SOLVER_URL,
                    username,
                    maxAttempts: 3,
                    infinite: false,
                    log: (...a) => {
                        if (!botAlive()) return;
                        plog(a.join(' '));
                    },
                })
                    .then(() => {
                        captchaBusy = false;
                        plog('капча ок → /l');
                    })
                    .catch((e) => {
                        captchaBusy = false;
                        plog(`captcha: ${e.message}`);
                        if (!settled) {
                            clearTimeout(timer);
                            done({ status: 'error', reason: `captcha: ${e.message}`.slice(0, 240) });
                        }
                    });
                return;
            }
            if (text.toLowerCase().includes('чтобы двигаться')) {
                funauthBindRequired = true;
                cancelFunauthVerify();
                plog('хуйня → funauth bind, ждём');
                requestFunauthBindHttp(username, password, an ? Number(an) : undefined);
                if (joinedAnarchy && !settled) {
                    clearTimeout(timer);
                    done({ status: 'ok' });
                }
                return;
            }
            if (
                text.toLowerCase().includes('подтвердите вход через')
                || (text.toLowerCase().includes('личные сообщения')
                    && text.toLowerCase().includes('подтвердите'))
            ) {
                plog('ВК/ТГ → funauth /2fa only');
                requestFunauthTwoFaHttp(username, an ? Number(an) : undefined);
                return;
            }
            if (
                text.includes('Зарегистрируйтесь')
                || text.includes('/reg <')
                || text.includes('/reg<')
            ) {
                bot.chat(`/reg ${password}`);
                setTimeout(() => bot.chat(`/l ${password}`), 600);
                return;
            }
            if (text.includes('Сначала авторизируйтесь') || text.includes('Авторизируйтесь')) {
                bot.chat(`/l ${password}`);
            }
        };

        bot.on('login', () => plog('login'));

        bot.on('kicked', (reason) => {
            const text = flattenKickReason(reason);
            lastKick = text;
            plog(`kicked: ${text.slice(0, 240)}`);
            if (isNickAlreadyOnlineText(text)) {
                clearTimeout(timer);
                done({ status: 'ok', reason: 'ник уже онлайн (другой клиент)' });
            }
            // иначе не финалим — бан только из чата «ВЫ ЗАБАНЕНЫ!»
        });

        bot.on('end', () => {
            if (settled) return;
            // ждём чат-бан: FunTime часто шлёт messagestr чуть раньше/рядом с disconnect
            setTimeout(() => {
                if (settled) return;
                clearTimeout(timer);
                done({
                    status: 'error',
                    reason: lastKick
                        ? `end after kick: ${lastKick.slice(0, 200)}`
                        : 'connection end',
                });
            }, 1500);
        });

        bot.on('error', (err) => {
            if (settled) return;
            plog(`error: ${err.message}`);
        });

        bot.on('messagestr', (msg) => onChatText(msg));
        bot._client?.on('systemChat', (data) => onChatText(packetToText(data)));
        bot._client?.on('playerChat', (data) => onChatText(packetToText(data)));

        bot.on('scoreboardCreated', (scoreboard) => {
            if (!an || settled) return;
            try {
                if (JSON.stringify(scoreboard).includes(an)) {
                    if (joinedAnarchy) return;
                    joinedAnarchy = true;
                    plog(`на анархии ${an} — funauth check 5с`);
                    funauthBindRequired = false;
                    cancelFunauthVerify();
                    funauthVerifyTimer = setTimeout(() => {
                        funauthVerifyTimer = null;
                        if (settled) return;
                        if (!funauthBindRequired) {
                            plog('FunAuth уже привязан — 5с без «чтобы двигаться»');
                            requestFunauthVerifiedHttp(username, an ? Number(an) : undefined);
                        }
                        clearTimeout(timer);
                        done({ status: 'ok' });
                    }, 5000);
                }
            } catch { /* ignore */ }
        });

        bot.once('spawn', async () => {
            plog('spawn lobby — идём на анку (бан ловится там)');
            try {
                bot.chat(`/l ${password}`);
            } catch { /* ignore */ }
            await sleep(1500);
            if (settled) return;
            if (!an) {
                clearTimeout(timer);
                done({ status: 'error', reason: 'нет anarchy' });
                return;
            }

            for (let i = 0; i < 10 && !settled && !joinedAnarchy; i++) {
                if (bot._client?.state === 'configuration') {
                    await sleep(500);
                    continue;
                }
                plog(`/an${an}…`);
                bot.physicsEnabled = false;
                bot.chat(`/an${an}`);
                await sleep(4500);
            }

            // бан-чат иногда чуть позже последнего /an
            if (!settled) await sleep(6000);
            if (!settled) {
                if (!joinedAnarchy) {
                    clearTimeout(timer);
                    done({
                        status: 'error',
                        reason: `не вошли на an${an} (нет бана в чате и нет scoreboard)`,
                    });
                }
                // joinedAnarchy: done() после 5с funauth check или после «чтобы двигаться»
            }
        });
    });
}
