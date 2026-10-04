/**
 * Владелец клана: логин → капча → /an → create → (если есть myNick: /clan info + kick чужих)
 * → invite только тех, кого ещё нет в clan info → права в /clan menu (всем).
 * Крутится сам до цели (кик/прокси/ошибка → пауза 5с → снова). Цель → exit 0.
 *
 *   node scripts/clan-setup.mjs <anarchy> [myNick]
 *   node scripts/clan-setup.mjs 504
 *
 * myNick: CLI > clan-owners.json[an].myNick > clan-owners.json.myNick
 */
import { readFileSync, existsSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';
import mineflayer from 'mineflayer';
import { SocksClient } from 'socks';
import { SocksProxyAgent } from 'socks-proxy-agent';
import { handleCaptchaLogin, attachMapCache, isCaptchaChat } from '../lib/captcha/solve-flow.mjs';
import { antiAfkIfNeeded, lookAroundSpin } from '../lib/afk-look.mjs';
import { patchWalking121 } from '../lib/walk-121.mjs';
import { loadClanOwnerSession } from '../lib/owner-proxy.mjs';
import { extractBanReason, isBanChatText } from '../lib/clan-owner-ping.mjs';
import { reportClanOwnerToGo } from '../lib/clan-owner-go.mjs';
import { proxyHostFromString } from '../lib/proxy-host.mjs';
import { awaitFleetLaunchGrant, cancelFleetLaunch } from '../lib/fleet-launch-gate.mjs';
import { installFuntimeKeepAlive, FUNTIME_KEEPALIVE_BOT_OPTS } from '../lib/funtime-keepalive.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');

const CLAN_CREATE_OK = '[⚔] Супер! Вы успешно создали клан:';
const CLAN_ALREADY = '[⚔] Ошибка: Ты уже состоишь в клане!';
const CLAN_OTHER = '[⚔] Ошибка: Игрок состоит в другом клане!'; // FunTime: и чужой клан, и уже наш
const CLAN_INVITE_OK = '[⚔] Вы отправили приглашение в клан игроку';
const CLAN_OFFLINE_HEAD = '[⚔] Ошибка: Игрок';
const CLAN_OFFLINE_TAIL = ' не в сети!';
/** «[⚔] Игрок nick присоединился к клану!» */
const CLAN_JOINED_RE = /Игрок\s+(\S+)\s+присоединился к клану/i;
/** Сколько ждать принятия инвайта после «приглашение отправлено». */
const JOIN_WAIT_MS = 60_000;
const AFK_MARKER = 'Данная команда недоступна в режиме AFK';

const LMB = 0;
const SHIFT = 1;
/** В /clan menu — кнопка списка участников */
const MEMBERS_MENU_SLOT = 11;
/** В окне прав — слот для финального shift (как sellbot) */
const RIGHTS_GRANT_SLOT = 11;
const KICK_CONFIRM_SLOT = 0;
const CLAN_MEMBERS_MARKER = 'Участники:';
const MEMBER_NAME_RE = /\][\s]*([a-zA-Z0-9_]{3,16})/g;
const SOLVER_URL = process.env.CAPTCHA_SOLVER_URL || 'http://127.0.0.1:8799';
const GO_HTTP = process.env.GO_HTTP_URL
    || (process.env.LOCAL_MODE === '1' || process.env.LOCAL_MODE === 'true'
        ? 'http://127.0.0.1:8080'
        : 'http://212.8.229.76:8080');

function requestFunauthBindHttp(nick, password, anarchy) {
    if (!nick || !password) return;
    fetch(`${GO_HTTP}/api/funauth/bind`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ nick, password, anarchy }),
    }).catch((e) => console.warn(`[clan-setup] funauth bind: ${e.message}`));
}

/** Только `/2fa` с TG, к которому ник уже привязан (без /bind). */
function requestFunauthTwoFaHttp(nick, anarchy) {
    if (!nick) return;
    fetch(`${GO_HTTP}/api/funauth/2fa`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ nick, anarchy }),
    }).catch((e) => console.warn(`[clan-setup] funauth 2fa: ${e.message}`));
}

/** FunTime: нужен TG/ВК confirm или /2fa через FunAuthBot. */
function isVkTgLoginConfirm(text) {
    const s = String(text || '').toLowerCase().replace(/ё/g, 'е');
    return (
        s.includes('подтвердите вход через')
        || (s.includes('личные сообщения') && s.includes('подтвердите'))
    );
}

/** FunTime: акк ещё не привязан к TG — надо /tg + FunAuth bind, сессию рвём. */
function isTgBindRequired(text) {
    const s = String(text || '').toLowerCase().replace(/ё/g, 'е');
    if (!s) return false;
    if (s.includes('чтобы двигаться')) return true;
    if (s.includes('после привязки')) return true;
    if (s.includes('защитит') && (s.includes('аккаунт') || s.includes('взлом'))) return true;
    if (s.includes('/tg') && (s.includes('привяз') || s.includes('телеграм') || s.includes('вк'))) return true;
    return false;
}

const CONFIG_BLOCKED = new Set([
    'position', 'look', 'position_look', 'flying',
    'chat', 'chat_command', 'chat_command_signed', 'chat_message',
    'window_click', 'close_window',
    'arm_animation', 'entity_action',
    'held_item_slot', 'set_creative_slot',
]);

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function rnd(min, max) {
    return sleep(min + Math.floor(Math.random() * (max - min + 1)));
}

function log(msg) {
    console.log(`[clan-setup] ${msg}`);
}

function parseArgs(argv) {
    let anarchy = null;
    let me = null;
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--anarchy' || a === '-a') anarchy = argv[++i];
        else if (a === '--me' || a === '--nick' || a === '-m') me = argv[++i];
        else if (/^\d{3}$/.test(a) && !anarchy) anarchy = a;
        else if (!a.startsWith('-') && !me) me = a;
    }
    if (!anarchy) {
        console.error('usage: node scripts/clan-setup.mjs <anarchy> [myNick]');
        process.exit(2);
    }
    return { anarchy: String(anarchy), me: me ? String(me) : null };
}

function loadJson(path) {
    return JSON.parse(readFileSync(path, 'utf8'));
}

function flattenChat(raw) {
    if (raw == null) return '';
    if (typeof raw === 'string') {
        try {
            return flattenChat(JSON.parse(raw));
        } catch {
            return raw;
        }
    }
    if (typeof raw !== 'object') return String(raw);
    let out = raw.text ?? raw.translate ?? '';
    if (Array.isArray(raw.extra)) {
        for (const p of raw.extra) out += flattenChat(p);
    }
    if (Array.isArray(raw.with)) {
        for (const p of raw.with) out += flattenChat(p);
    }
    return out;
}

function packetToText(data) {
    if (data?.plainMessage) return String(data.plainMessage);
    return flattenChat(data?.formattedMessage ?? data?.content ?? data?.unsignedContent ?? data);
}

function randomClanName() {
    const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789';
    let s = '';
    for (let i = 0; i < 5; i++) {
        s += alphabet[Math.floor(Math.random() * alphabet.length)];
    }
    return s;
}

function nickIn(text, nick) {
    return String(text).toLowerCase().includes(String(nick).toLowerCase());
}

/** «Участники: [Лидер]nick, [Участник]nick2» → lowercase nicks */
function parseClanMembersFromChat(text) {
    if (!text?.includes(CLAN_MEMBERS_MARKER)) return null;
    const idx = text.indexOf(CLAN_MEMBERS_MARKER);
    const tail = text.slice(idx + CLAN_MEMBERS_MARKER.length);
    const names = [];
    let m;
    MEMBER_NAME_RE.lastIndex = 0;
    while ((m = MEMBER_NAME_RE.exec(tail)) !== null) {
        names.push(m[1].toLowerCase());
    }
    return names.length ? names : null;
}

function findClanIntruders(members, allowedLower) {
    const allowed = new Set(allowedLower.map((n) => String(n).toLowerCase()));
    return members.filter((m) => !allowed.has(m));
}

function ensureRegistryDimensionStub(bot) {
    if (Array.isArray(bot.registry.dimensionsArray) && bot.registry.dimensionsArray.length > 0) {
        return;
    }
    const fallback = { name: 'minecraft:overworld', minY: -64, height: 384 };
    bot.registry.dimensionsArray = Array.from({ length: 16 }, () => fallback);
    bot.registry.dimensionsByName ??= { overworld: fallback };
}

function setupConfigurationTransferFix(bot) {
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

    const PACK_ACCEPTED = 3;
    const PACK_LOADED = 0;
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
        origWrite('resource_pack_receive', { uuid: data.uuid, result: PACK_ACCEPTED });
        origWrite('resource_pack_receive', { uuid: data.uuid, result: PACK_LOADED });
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

async function closeWindow(bot) {
    const win = bot?.currentWindow;
    if (!win) return;
    try {
        await bot.closeWindow(win);
    } catch {
        /* ignore */
    }
    await sleep(300);
}

function buildProxyConnect(proxyString) {
    const url = new URL(proxyString);
    const proxyHost = url.hostname;
    const proxyPort = Number(url.port);
    const proxyUsername = url.username || undefined;
    const proxyPassword = url.password || undefined;
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

async function runSession({ anarchy, me, owner, proxyString, inviteNicks, requiredBotNicks, allowedNicks }) {
    let sessionReject = null;
    let finishedOk = false;

    log(`owner=${owner.username} an=${anarchy} me=${me || '(нет — только боты)'}`);
    log(`invite: ${inviteNicks.join(', ') || '(пусто)'}`);
    log(`solver: ${SOLVER_URL}`);

    const granted = await awaitFleetLaunchGrant({
        username: owner.username,
        anarchy,
        kind: 'owner',
        log,
    });
    if (!granted) {
        throw new Error('launch-gate aborted');
    }

    const state = {
        afk: false,
        aborted: false,
        timeJoinAnarchy: 0,
        captchaBusy: false,
        createOk: false,
        alreadyInClan: false,
        lastInvite: null,
        inviteOk: false,
        inviteOtherClan: false,
        inviteOffline: false,
        /** lowercase nicks that sent «присоединился к клану» */
        joinedNicks: new Set(),
        menu: null,
        guiBusy: false,
        clanMembersSnapshot: null,
        kickDone: false,
        funauthRequested: false,
        grantCurrent: null,
        grantOk: false,
        grantMemberSlot: null,
        shift2At: 0,
        grantScanOnly: false,
        ownerOkReported: false,
    };

    const proxy = buildProxyConnect(proxyString);
    log(`прокси ${proxy.label}`);

    const bot = mineflayer.createBot({
        username: owner.username,
        password: owner.password,
        host: 'mc.funtime.su',
        port: 25565,
        version: '1.21.11',
        physicsEnabled: false,
        hideErrors: true,
        logErrors: false,
        ...FUNTIME_KEEPALIVE_BOT_OPTS,
        agent: proxy.agent,
        connect: proxy.connect,
    });
    installFuntimeKeepAlive(bot);

    const failSession = (msg) => {
        if (finishedOk || state.aborted) return;
        state.aborted = true;
        console.error(`[clan-setup] ${msg}`);
        try {
            bot.quit();
        } catch {
            /* ignore */
        }
        if (sessionReject) {
            const rej = sessionReject;
            sessionReject = null;
            rej(new Error(msg));
        }
    };

    const dead = new Promise((_, reject) => {
        sessionReject = reject;
    });

    attachMapCache(bot);
    patchWalking121(bot, { tickEndEveryTick: true });
    setupConfigurationTransferFix(bot);

    bot.on('scoreboardCreated', (scoreboard) => {
        if (JSON.stringify(scoreboard).includes(String(anarchy))) {
            state.timeJoinAnarchy = Date.now();
            log(`на анархии ${anarchy}`);
            if (!state.ownerOkReported) {
                state.ownerOkReported = true;
                reportClanOwnerToGo({
                    username: owner.username,
                    anarchy,
                    status: 'ok',
                    banned: false,
                    ip: proxyHostFromString(proxyString),
                });
            }
        }
    });

    bot.on('kicked', (reason) => {
        const text = flattenChat(reason) || JSON.stringify(reason);
        if (isBanChatText(text)) {
            const banReason = extractBanReason(text) || 'ВЫ ЗАБАНЕНЫ!';
            reportClanOwnerToGo({
                username: owner.username,
                anarchy,
                status: 'banned',
                banned: true,
                reason: banReason,
                ip: proxyHostFromString(proxyString),
            });
            failSession(`banned: ${banReason.slice(0, 220)}`);
            // Не retry: повторный логин забаненного owner → FunAC 4.3.1
            log(`BAN — stop clan-setup (no retry)`);
            process.exit(2);
            return;
        }
        failSession(`kicked ${JSON.stringify(reason)}`);
    });
    bot.on('error', (err) => {
        failSession(`error ${err.message}`);
    });
    bot.on('end', () => {
        failSession('connection end');
    });

    const onChatText = async (text) => {
        if (!text) return;
        const line = String(text);
        // баннер/auth — шум; дубли с systemChat не пишем
        if (
            !/^[╔╚║═\s]+$/.test(line)
            && !line.includes('★ FunTime')
            && !line.includes('Добро пожаловать')
            && !line.includes('социальные сети')
            && !line.includes('новых побед')
            && !line.includes('Успешная авторизация')
            && !line.includes('уже авторизованы')
        ) {
            console.log(`[clan-setup] 💬 ${line}`);
        }

        if (isBanChatText(text)) {
            const reason = extractBanReason(text) || 'ВЫ ЗАБАНЕНЫ!';
            reportClanOwnerToGo({
                username: owner.username,
                anarchy,
                status: 'banned',
                banned: true,
                reason,
                ip: proxyHostFromString(proxyString),
            });
            failSession(`banned: ${reason.slice(0, 220)}`);
            log(`BAN — stop clan-setup (no retry)`);
            process.exit(2);
            return;
        }

        if (/отключите\s+vpn|vpn и proxy|proxy и повторите/i.test(text)) {
            failSession('funtime_vpn_proxy_block');
            return;
        }

        if (text.includes(AFK_MARKER)) {
            state.afk = true;
            log('AFK');
            return;
        }

        if (isCaptchaChat(text)) {
            if (state.captchaBusy) return;
            state.captchaBusy = true;
            try {
                log('капча…');
                await handleCaptchaLogin(bot, {
                    password: owner.password,
                    solverUrl: SOLVER_URL,
                    username: owner.username,
                    infinite: true,
                    log: (...a) => log(a.join(' ')),
                });
                log('капча ок → /reg /l');
            } catch (e) {
                console.error('[clan-setup] captcha:', e.message, '— жду следующую капчу');
            } finally {
                state.captchaBusy = false;
            }
            return;
        }

        if (
            text.includes('Зарегистрируйтесь')
            || text.includes('/reg <')
            || text.includes('/reg<')
        ) {
            if (state.funauthRequested) return;
            bot.chat(`/reg ${owner.password}`);
            await rnd(400, 800);
            bot.chat(`/l ${owner.password}`);
            return;
        }
        if (text.includes('Вы уже авторизованы')) {
            return;
        }
        if (
            text.includes('Сначала авторизируйтесь')
            || text.includes('Авторизируйтесь')
        ) {
            // пока ждём ВК/ТГ — /l только спамит «уже авторизованы»
            if (state.funauthRequested) return;
            bot.chat(`/l ${owner.password}`);
            return;
        }

        if (isVkTgLoginConfirm(text)) {
            if (state.funauthRequested) return;
            state.funauthRequested = true;
            log('ВК/ТГ confirm → FunAuth /2fa (без bind) → убиваем сессию');
            requestFunauthTwoFaHttp(owner.username, Number(anarchy));
            failSession('vk/tg confirm (funauth 2fa)');
            return;
        }

        if (isTgBindRequired(text)) {
            if (state.funauthRequested) return;
            state.funauthRequested = true;
            log('TG bind required → FunAuth bind → выходим');
            requestFunauthBindHttp(owner.username, owner.password, Number(anarchy));
            failSession('хуйня неведомая (funauth)');
            return;
        }

        if (text.includes(CLAN_CREATE_OK)) {
            state.createOk = true;
            log('клан создан');
        }
        if (text.includes(CLAN_ALREADY)) {
            state.alreadyInClan = true;
            log('уже в клане — create stop');
        }

        if (state.lastInvite) {
            const nick = state.lastInvite;
            if (text.includes(CLAN_INVITE_OK) && nickIn(text, nick)) {
                state.inviteOk = true;
                log(`invite ok → ${nick}`);
            }
            // FunTime: и «уже в нашем», и «в чужом» → одна строка
            if (text.includes(CLAN_OTHER)) {
                state.inviteOtherClan = true;
                log(`invite skip (уже в клане / другой клан) → ${nick}`);
            }
            if (
                text.includes(CLAN_OFFLINE_HEAD)
                && text.includes(CLAN_OFFLINE_TAIL)
                && nickIn(text, nick)
            ) {
                state.inviteOffline = true;
                log(`invite skip (офлайн) → ${nick}`);
            }
        }

        const joined = text.match(CLAN_JOINED_RE);
        if (joined) {
            const who = joined[1];
            state.joinedNicks.add(String(who).toLowerCase());
            log(`joined → ${who}`);
        }

        const members = parseClanMembersFromChat(text);
        if (members != null) {
            state.clanMembersSnapshot = members;
            log(`clan info: ${members.join(', ')}`);
        }
    };

    bot.on('messagestr', (msg) => {
        void onChatText(String(msg || ''));
    });
    // systemChat/playerChat дублируют messagestr → в логе всё дважды

    bot.on('windowOpen', () => {
        void onWindowOpen(bot, state);
    });

    const work = (async () => {
        await new Promise((resolve, reject) => {
            const t = setTimeout(() => reject(new Error('spawn timeout')), 90_000);
            bot.once('spawn', () => {
                clearTimeout(t);
                resolve();
            });
        });

        bot.physicsEnabled = true;
        log('spawn → /reg → /l');
        await rnd(800, 1600);
        bot.chat(`/reg ${owner.password}`);
        await rnd(600, 1200);
        bot.chat(`/l ${owner.password}`);
        await rnd(1500, 2500);

        for (let i = 0; i < 40 && !state.timeJoinAnarchy && !state.aborted; i++) {
            if (bot._client?.state === 'configuration') {
                await sleep(500);
                continue;
            }
            await antiAfkIfNeeded(bot, state, log);
            log(`/an${anarchy}…`);
            bot.physicsEnabled = false;
            bot.chat(`/an${anarchy}`);
            await sleep(4000);
        }
        if (state.aborted) throw new Error('хуйня неведомая (funauth)');
        if (!state.timeJoinAnarchy) {
            throw new Error(`не вошли на анархию ${anarchy}`);
        }
        // без паузы 11с — это только для АХ; клану она не нужна
        bot.physicsEnabled = true;
        await lookAroundSpin(bot, log);

        // Уже в клане? не спамить create
        {
            state.clanMembersSnapshot = null;
            bot.chat('/clan info');
            const infoDeadline = Date.now() + 12_000;
            while (
                !state.alreadyInClan
                && !state.createOk
                && !state.aborted
                && state.clanMembersSnapshot == null
                && Date.now() < infoDeadline
            ) {
                await sleep(400);
            }
            if (state.alreadyInClan || (state.clanMembersSnapshot && state.clanMembersSnapshot.length)) {
                state.alreadyInClan = true;
                log('уже в клане (по /clan info) — create skip');
            }
        }

        const createDeadline = Date.now() + 90_000;
        let createTries = 0;
        while (!state.createOk && !state.alreadyInClan && !state.aborted && Date.now() < createDeadline) {
            if (!state.timeJoinAnarchy) {
                throw new Error('не на анархии — create abort');
            }
            await antiAfkIfNeeded(bot, state, log);
            if (state.aborted) break;
            if (state.afk) {
                await sleep(1000);
                continue;
            }
            createTries += 1;
            if (createTries > 8) {
                throw new Error('не удалось создать клан (too many tries)');
            }
            const name = randomClanName();
            // без кавычек: FunTime считает "hdblz" за >5 символов
            const cmd = `/clan create ${name}`;
            log(cmd);
            await closeWindow(bot);
            bot.chat(cmd);
            const waitUntil = Date.now() + 8_000;
            while (
                Date.now() < waitUntil
                && !state.createOk
                && !state.alreadyInClan
                && !state.aborted
            ) {
                await antiAfkIfNeeded(bot, state, log);
                await sleep(400);
            }
        }
        if (state.aborted) throw new Error('хуйня неведомая (funauth)');
        if (!state.createOk && !state.alreadyInClan) {
            throw new Error('не удалось создать клан');
        }

        // С основным ником — сначала вычищаем чужих из клана
        if (me) {
            await purgeIntruders(bot, state, allowedNicks);
        }

        // Кто уже в clan info — не инвайтим; права всё равно проверим всем ниже
        let alreadyIn = new Set(
            (state.clanMembersSnapshot || []).map((n) => String(n).toLowerCase()),
        );
        if (!alreadyIn.size) {
            const members = await safeClanInfo(bot, state);
            alreadyIn = new Set((members || []).map((n) => String(n).toLowerCase()));
        }
        if (alreadyIn.size) {
            log(`уже в клане (invite skip): ${[...alreadyIn].join(', ')}`);
        }

        const inviteOne = async (nick) => {
            if (alreadyIn.has(String(nick).toLowerCase())) {
                log(`⊘ invite skip ${nick} — уже в clan info`);
                return;
            }
            state.lastInvite = nick;
            state.inviteOk = false;
            state.inviteOtherClan = false;
            state.inviteOffline = false;
            const deadline = Date.now() + 90_000;
            while (
                Date.now() < deadline
                && !state.inviteOk
                && !state.inviteOtherClan
                && !state.inviteOffline
            ) {
                await antiAfkIfNeeded(bot, state, log);
                if (state.afk) {
                    await sleep(1000);
                    continue;
                }
                await closeWindow(bot);
                log(`/clan invite ${nick}`);
                bot.chat(`/clan invite ${nick}`);
                const waitUntil = Date.now() + 6_000;
                while (
                    Date.now() < waitUntil
                    && !state.inviteOk
                    && !state.inviteOtherClan
                    && !state.inviteOffline
                ) {
                    await antiAfkIfNeeded(bot, state, log);
                    await sleep(400);
                }
            }
            if (state.inviteOk) {
                log(`✓ invite ${nick} — ждём «присоединился» до ${JOIN_WAIT_MS / 1000}с`);
                const joinDeadline = Date.now() + JOIN_WAIT_MS;
                const key = String(nick).toLowerCase();
                while (Date.now() < joinDeadline && !state.joinedNicks.has(key)) {
                    await antiAfkIfNeeded(bot, state, log);
                    await sleep(400);
                }
                if (state.joinedNicks.has(key)) {
                    log(`✓ joined ${nick}`);
                    alreadyIn.add(key);
                } else {
                    log(`? join timeout ${nick} — идём дальше`);
                }
            } else if (state.inviteOtherClan) {
                log(`⊘ ${nick} уже в клане / другой клан`);
            } else if (state.inviteOffline) {
                log(`⊘ ${nick} офлайн`);
            } else {
                log(`? invite timeout ${nick}`);
            }
            await rnd(800, 1500);
        };

        // requiredBots — только ферма из bots/*.json; myNick не считается «успехом»
        const requiredBots = (Array.isArray(requiredBotNicks) ? requiredBotNicks : inviteNicks)
            .map((n) => String(n || '').trim())
            .filter(Boolean);

        for (let round = 0; round < 3; round++) {
            const missing = requiredBots.filter((n) => !alreadyIn.has(String(n).toLowerCase()));
            if (!missing.length) break;
            if (round > 0) {
                log(`re-invite round ${round + 1}/3 missing=${missing.join(', ')}`);
                await rnd(2000, 4000);
            }
            for (const nick of round === 0 ? inviteNicks : missing) {
                await inviteOne(nick);
            }
            const members = (await safeClanInfo(bot, state)) || [];
            alreadyIn = new Set(members.map((n) => String(n).toLowerCase()));
            log(`clan info after invites: ${members.join(', ') || '(пусто)'}`);
        }

        {
            const missing = requiredBots.filter((n) => !alreadyIn.has(String(n).toLowerCase()));
            if (missing.length) {
                log(`⚠ incomplete roster (права всё равно): missing ${missing.join(', ')}`);
            }
        }

        // Права тем, кто уже в клане — не ждём офлайн tip9/qxEmber
        await grantAllRights(bot, state, inviteNicks, owner.username, requiredBotNicks);

        {
            const missing = requiredBots.filter((n) => !alreadyIn.has(String(n).toLowerCase()));
            if (missing.length) {
                // права живым уже выдали; incomplete → Go снова позовёт когда офлайны оживут
                throw new Error(`clan incomplete after grants: missing ${missing.join(', ')}`);
            }
        }

        log('цель достигнута');
        finishedOk = true;
        sessionReject = null;
    })();

    try {
        await Promise.race([work, dead]);
    } finally {
        finishedOk = true;
        sessionReject = null;
        try {
            await cancelFleetLaunch({ username: owner.username, anarchy, log });
        } catch {
            /* ignore */
        }
        try {
            bot.removeAllListeners();
            bot.quit();
        } catch {
            /* ignore */
        }
    }
}

async function main() {
    const { anarchy, me: meArg } = parseArgs(process.argv.slice(2));
    const session = loadClanOwnerSession(ROOT, anarchy);
    if (!session) {
        console.error(`нет владельца/прокси для ${anarchy} (clan-owners.json + owner-ip.json)`);
        process.exit(2);
    }
    const { owner, proxyString } = session;
    const owners = loadJson(join(ROOT, 'clan-owners.json'));

    const me = String(
        meArg
        || owner.myNick
        || owners.myNick
        || '',
    ).trim() || null;
    if (me) log(`myNick: ${me}`);
    else log('myNick: нет — purge чужих выключен');

    const botsPath = join(ROOT, 'bots', `${anarchy}b.json`);
    const bots = existsSync(botsPath) ? loadJson(botsPath) : [];
    const inviteNicks = [];
    const requiredBotNicks = [];
    const seen = new Set();
    const pushNick = (n, intoRequired = false) => {
        const nick = String(n || '').trim();
        if (!nick) return;
        const key = nick.toLowerCase();
        if (seen.has(key)) return;
        if (key === String(owner.username).toLowerCase()) return;
        seen.add(key);
        inviteNicks.push(nick);
        if (intoRequired) requiredBotNicks.push(nick);
    };
    pushNick(me, false);
    for (const b of bots) pushNick(b?.username, true);

    const allowedNicks = [
        owner.username,
        me,
        ...bots.map((b) => b?.username),
    ].filter(Boolean);

    for (let n = 1; ; n++) {
        log(`═══ сессия #${n} ═══`);
        try {
            await runSession({
                anarchy,
                me,
                owner,
                proxyString,
                inviteNicks,
                requiredBotNicks,
                allowedNicks,
            });
            log('готово — выход');
            process.exit(0);
        } catch (e) {
            const msg = String(e?.message || e);
            if (/^banned:/i.test(msg) || /ВЫ ЗАБАНЕНЫ/i.test(msg)) {
                log(`BAN — stop clan-setup (no retry): ${msg.slice(0, 120)}`);
                process.exit(2);
            }
            if (/funtime_vpn_proxy|vpn_proxy_block/i.test(msg)) {
                log(`VPN/Proxy block FunTime — stop clan-setup (смени owner SOCKS): ${msg.slice(0, 120)}`);
                process.exit(3);
            }
            // Parse/protocol desync — бесконечный ретрай только засоряет лог
            if (/array size is abnormally large|Parse error for play\.toClient/i.test(msg) && n >= 3) {
                log(`protocol fail ×${n} — stop clan-setup: ${msg.slice(0, 160)}`);
                process.exit(4);
            }
            // incomplete — мягкий ретрай; FunAuth — дольше
            let waitMs = 5_000;
            if (/хуйня|funauth|vk\/tg|confirm/i.test(msg)) waitMs = 45_000;
            else if (/clan incomplete/i.test(msg)) waitMs = 12_000;
            log(`сбой: ${msg} — через ${Math.round(waitMs / 1000)}с снова`);
            await sleep(waitMs);
        }
    }
}

async function safeClanInfo(bot, state, waitMs = 25_000) {
    state.clanMembersSnapshot = null;
    const deadline = Date.now() + waitMs;
    while (state.clanMembersSnapshot == null && Date.now() < deadline) {
        await closeWindow(bot);
        log('/clan info…');
        bot.chat('/clan info');
        await sleep(3500);
        if (state.clanMembersSnapshot == null) {
            await antiAfkIfNeeded(bot, state, log);
        }
    }
    return state.clanMembersSnapshot;
}

async function kickFromClan(bot, state, nick, deadline) {
    const cmd = `/clan kick ${nick}`;
    while (Date.now() < deadline) {
        state.kickDone = false;
        state.menu = 'clan_kick';
        await closeWindow(bot);
        log(cmd);
        bot.chat(cmd);

        const roundEnd = Math.min(deadline, Date.now() + 12_000);
        while (Date.now() < roundEnd && !state.kickDone) {
            await sleep(400);
            if (!bot.currentWindow) await antiAfkIfNeeded(bot, state, log);
        }

        if (state.kickDone) {
            await sleep(800);
            await closeWindow(bot);
            state.menu = null;
            return true;
        }
        log('kick — повтор');
        await sleep(1500);
    }
    state.menu = null;
    return false;
}

/** /clan info → kick всех, кого нет в JSON и кто не основной ник / не владелец */
async function purgeIntruders(bot, state, allowedNicks) {
    const allowed = allowedNicks.map((n) => String(n).toLowerCase());
    log(`purge: оставляем ${allowed.join(', ')}`);
    const deadline = Date.now() + 180_000;

    while (Date.now() < deadline) {
        await antiAfkIfNeeded(bot, state, log);
        const members = await safeClanInfo(bot, state);
        if (!members?.length) {
            log('clan info — нет участников, повтор');
            await sleep(2000);
            continue;
        }

        const intruders = findClanIntruders(members, allowed);
        if (!intruders.length) {
            log(`clan info: чужих нет (${members.length} уч.)`);
            return;
        }

        for (const name of intruders) {
            if (Date.now() >= deadline) return;
            log(`лишний ${name} — kick`);
            await kickFromClan(bot, state, name, Math.min(deadline, Date.now() + 30_000));
            await sleep(1200);
        }
    }
    log('purge: таймаут');
}

async function onWindowOpen(bot, state) {
    if (!bot?.currentWindow || state.menu == null || state.guiBusy) return;
    state.guiBusy = true;
    try {
        if (state.menu === 'clan_kick') {
            await rnd(600, 1200);
            if (!bot.currentWindow) return;
            log(`kick confirm слот ${KICK_CONFIRM_SLOT}`);
            state.menu = null;
            state.kickDone = true;
            await bot.clickWindow(KICK_CONFIRM_SLOT, LMB, 0);
            return;
        }
        if (state.menu === 'clan_menu') {
            await rnd(800, 1600);
            if (!bot.currentWindow) return;
            log(`клик слот ${MEMBERS_MENU_SLOT} (members)`);
            state.menu = 'clan_members';
            state.guiBusy = false;
            await bot.clickWindow(MEMBERS_MENU_SLOT, LMB, 0);
            return;
        }
        // участники → shift×1 по слоту головы → окно прав
        if (state.menu === 'clan_members') {
            await rnd(700, 1400);
            if (!bot.currentWindow) return;
            // только смотрим головы (discover) — кликать не надо
            if (state.grantScanOnly) {
                return;
            }
            const slot = Number(state.grantMemberSlot);
            if (!Number.isInteger(slot) || slot < 0 || !bot.currentWindow.slots?.[slot]) {
                log(`слот головы ${slot}: пусто — dump`);
                dumpMemberSlots(bot);
                state.menu = 'grant_miss';
                return;
            }
            const label = itemLabel(bot.currentWindow.slots[slot]) || `slot${slot}`;
            log(`shift×1 слот ${slot} (${label}) → окно прав`);
            state.menu = 'clan_shift2';
            state.shift2At = Date.now() + 700;
            state.guiBusy = false;
            await bot.clickWindow(slot, LMB, SHIFT);
            return;
        }
        // права → shift×2 (слот 11 как в sellbot)
        if (state.menu === 'clan_shift2') {
            await rnd(600, 1200);
            if (!bot.currentWindow) return;
            await doRightsShift2(bot, state);
        }
    } finally {
        state.guiBusy = false;
    }
}

function itemDump(item) {
    try {
        return JSON.stringify(item);
    } catch {
        return String(item);
    }
}

/** Все строки из item (1.21 components часто прячут ник не в одном поле). */
function collectStrings(node, out = []) {
    if (node == null) return out;
    if (typeof node === 'string') {
        if (node.trim()) out.push(node);
        return out;
    }
    if (typeof node === 'number' || typeof node === 'boolean') return out;
    if (Array.isArray(node)) {
        for (const x of node) collectStrings(x, out);
        return out;
    }
    if (typeof node === 'object') {
        for (const v of Object.values(node)) collectStrings(v, out);
    }
    return out;
}

function itemLabel(item) {
    if (!item) return '';
    const parts = collectStrings(item)
        .map((s) => s.replace(/\u00a7./g, '').trim())
        .filter(Boolean);
    // предпочитаем кусок похожий на ник
    const nickish = parts.find((s) => /^[A-Za-z0-9_]{3,16}$/.test(s));
    if (nickish) return nickish;
    return parts.slice(0, 3).join(' | ').slice(0, 80);
}

function findSlotWithNick(bot, nick) {
    const want = String(nick || '').toLowerCase();
    if (!want || !bot.currentWindow?.slots) return -1;
    const slots = bot.currentWindow.slots;
    for (let i = 0; i < Math.min(slots.length, 27); i++) {
        const item = slots[i];
        if (!item) continue;
        const blob = collectStrings(item).join(' ').toLowerCase();
        if (blob.includes(want)) return i;
    }
    return -1;
}

/** Головы участников в верхнем GUI (FunTime 1.21: type 1235, иногда name/head). */
function isMemberHeadItem(item) {
    if (!item) return false;
    if (item.type === 1235) return true;
    const n = String(item.name || '').toLowerCase();
    const d = String(item.displayName || '').toLowerCase();
    if (n.includes('head') || n.includes('skull') || d.includes('head') || d.includes('skull')) {
        return true;
    }
    // fallback: player profile в NBT/components часто есть у голов
    const blob = collectStrings(item).join(' ').toLowerCase();
    if (blob.includes('profile') || blob.includes('skullowner') || blob.includes('minecraft:profile')) {
        return true;
    }
    return false;
}

function listMemberHeadSlots(bot) {
    const slots = bot.currentWindow?.slots || [];
    const out = [];
    for (let i = 0; i < Math.min(slots.length, 27); i++) {
        const item = slots[i];
        if (isMemberHeadItem(item)) out.push(i);
    }
    return out;
}

function dumpMemberSlots(bot) {
    const slots = bot.currentWindow?.slots || [];
    for (let i = 0; i < Math.min(slots.length, 27); i++) {
        const item = slots[i];
        if (!item) continue;
        const label = itemLabel(item);
        log(`  slot ${i}: type=${item.type} label=${label || '?'}`);
    }
}

function rightsWindowStillHasNo(bot) {
    const slots = bot.currentWindow?.slots || [];
    for (let i = 0; i < Math.min(slots.length, 54); i++) {
        const item = slots[i];
        if (!item) continue;
        if (collectStrings(item).some((s) => s.includes('Нет'))) return true;
    }
    return false;
}

async function doRightsShift2(bot, state) {
    if (state.menu !== 'clan_shift2' || !bot?.currentWindow) return false;
    state.shift2At = 0;
    log(`shift×2 (права slot ${state.grantMemberSlot})`);

    const pickSlot = () => {
        const slots = bot.currentWindow?.slots || [];
        for (let i = 0; i < Math.min(slots.length, 54); i++) {
            const item = slots[i];
            if (item && collectStrings(item).some((s) => s.includes('Нет'))) return i;
        }
        if (state.grantMemberSlot != null && bot.currentWindow.slots?.[state.grantMemberSlot]) {
            return state.grantMemberSlot;
        }
        return RIGHTS_GRANT_SLOT;
    };

    for (let i = 0; i < 6; i++) {
        if (!bot.currentWindow) break;
        const slot = pickSlot();
        log(`  shift слот ${slot} #${i + 1}`);
        await bot.clickWindow(slot, LMB, SHIFT);
        await sleep(550);
        if (!rightsWindowStillHasNo(bot)) break;
        log('  ещё «Нет» — ещё shift');
    }
    state.menu = null;
    state.grantOk = true;
    await sleep(400);
    await closeWindow(bot);
    return true;
}

/**
 * /clan menu → участники → shift по каждой голове в GUI → shift в окне прав.
 * Выдаём всем не-лидерам, кто сейчас в clan info (офлайн из ростера не блокирует).
 */
async function grantAllRights(bot, state, grantNicks, ownerUsername, requiredBotNicks = []) {
    const ownerKey = String(ownerUsername || '').toLowerCase();
    const required = (requiredBotNicks || [])
        .map((n) => String(n || '').trim())
        .filter(Boolean);
    const members = (await safeClanInfo(bot, state)) || state.clanMembersSnapshot || [];
    const inClan = new Set(members.map((m) => String(m).toLowerCase()));

    const presentRequired = required.filter((n) => inClan.has(String(n).toLowerCase()));
    const missingRequired = required.filter((n) => !inClan.has(String(n).toLowerCase()));
    if (missingRequired.length) {
        log(`права: нет в клане (skip, выдаём кто есть): ${missingRequired.join(', ')}`);
    }
    if (!presentRequired.length && required.length) {
        log('права: ни одного ферм-бота в clan info — всё равно пройдём головы не-лидеров');
    }

    // Сколько голов ждём в GUI: только кто реально в клане сейчас (не весь ростер)
    const wantHeads = Math.max(
        0,
        members.filter((m) => String(m).toLowerCase() !== ownerKey).length,
    );
    if (wantHeads === 0) {
        log('права: в клане только лидер — нечего выдавать');
        return;
    }

    // открываем members — собираем слоты голов (несколько попыток)
    let headSlots = [];
    for (let attempt = 1; attempt <= 4; attempt++) {
        headSlots = await discoverMemberHeadSlots(bot, state);
        const nonOwner = headSlots.filter(
            (h) => !h.label || String(h.label).toLowerCase() !== ownerKey,
        );
        if (nonOwner.length >= wantHeads && wantHeads > 0) {
            break;
        }
        if (headSlots.length && wantHeads === 0) break;
        log(`головы attempt ${attempt}: got=${headSlots.length} nonOwner=${nonOwner.length} want≥${wantHeads}`);
        dumpMemberSlots(bot);
        await rnd(1500, 2500);
    }
    if (!headSlots.length) {
        dumpMemberSlots(bot);
        throw new Error('в /clan members нет голов участников');
    }
    const grantTargets = headSlots.filter(
        (h) => !h.label || String(h.label).toLowerCase() !== ownerKey,
    );
    if (!grantTargets.length) {
        dumpMemberSlots(bot);
        throw new Error(`нет голов не-лидеров для прав (members=${members.join(',') || '—'})`);
    }
    if (grantTargets.length < wantHeads) {
        // не фейлим сессию: выдадим сколько нашли (GUI мог не отрисовать всех)
        log(`⚠ голов меньше чем в info: ${grantTargets.length} < ${wantHeads} — выдаём найденные`);
    }
    log(`права по слотам голов: ${headSlots.map((s) => `${s.slot}:${s.label}`).join(', ')}`);

    for (const { slot, label } of headSlots) {
        if (label && label.toLowerCase() === ownerKey) {
            log(`skip лидер ${label}`);
            continue;
        }
        const ok = await grantOneMemberSlot(bot, state, slot, label);
        if (!ok) throw new Error(`не выдал права slot ${slot} (${label || '?'})`);
        log(`✓ права ${label || `slot${slot}`}`);
        await rnd(800, 1500);
    }
    log(`права выданы (heads=${grantTargets.length}, presentBots=${presentRequired.length}/${required.length})`);
}

async function discoverMemberHeadSlots(bot, state, waitMs = 40_000) {
    const deadline = Date.now() + waitMs;
    while (Date.now() < deadline) {
        state.menu = 'clan_menu';
        state.grantMemberSlot = null;
        state.grantOk = false;
        state.grantScanOnly = true;
        await closeWindow(bot);
        log('/clan menu → список голов');
        bot.chat('/clan menu');
        const roundEnd = Date.now() + 15_000;
        const roundStart = Date.now();
        let membersFallback = false;
        while (Date.now() < roundEnd) {
            await sleep(300);
            if (!bot.currentWindow) await antiAfkIfNeeded(bot, state, log);
            if (
                bot.currentWindow
                && state.menu === 'clan_menu'
                && !membersFallback
                && Date.now() - roundStart > 3500
            ) {
                try {
                    log(`клик слот ${MEMBERS_MENU_SLOT} (members, fallback)`);
                    state.menu = 'clan_members';
                    await bot.clickWindow(MEMBERS_MENU_SLOT, LMB, 0);
                    membersFallback = true;
                } catch {
                    /* ignore */
                }
            }
            if (bot.currentWindow && state.menu === 'clan_members') {
                await sleep(600);
                const slots = listMemberHeadSlots(bot);
                if (slots.length) {
                    const out = slots.map((slot) => ({
                        slot,
                        label: itemLabel(bot.currentWindow.slots[slot]),
                    }));
                    state.grantScanOnly = false;
                    await closeWindow(bot);
                    state.menu = null;
                    return out;
                }
                log('members GUI без голов — dump:');
                dumpMemberSlots(bot);
            }
        }
        log('головы не нашли — повтор меню');
        await sleep(1500);
    }
    state.grantScanOnly = false;
    return [];
}

async function grantOneMemberSlot(bot, state, slot, label) {
    const deadline = Date.now() + 90_000;
    let openedAt = 0;

    while (Date.now() < deadline) {
        state.grantCurrent = label || `slot${slot}`;
        state.grantMemberSlot = slot;
        state.grantOk = false;
        state.shift2At = 0;
        state.grantScanOnly = false;
        state.menu = 'clan_menu';
        await closeWindow(bot);
        log(`/clan menu → права ${state.grantCurrent} (slot ${slot})`);
        bot.chat('/clan menu');
        openedAt = Date.now();

        while (Date.now() < deadline && !state.grantOk) {
            await sleep(300);
            if (!bot.currentWindow) await antiAfkIfNeeded(bot, state, log);

            if (
                state.menu === 'clan_shift2'
                && bot.currentWindow
                && state.shift2At > 0
                && Date.now() >= state.shift2At
                && !state.guiBusy
            ) {
                state.guiBusy = true;
                try {
                    await doRightsShift2(bot, state);
                } finally {
                    state.guiBusy = false;
                }
                continue;
            }

            if (state.menu === 'grant_miss') {
                log(`права ${state.grantCurrent}: слот пропал — повтор`);
                break;
            }

            if (
                state.menu === 'clan_menu'
                && !bot.currentWindow
                && Date.now() - openedAt > 12_000
            ) {
                log(`права ${state.grantCurrent}: меню не открылось — повтор`);
                break;
            }
        }

        if (state.grantOk) return true;
        await sleep(1500);
    }
    return false;
}

main().catch((e) => {
    console.error('[clan-setup] fatal:', e.message);
    process.exit(1);
});
