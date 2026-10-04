const PLAY_SPAWN_ENTITY = 0x01;
const PLAY_BOSS_BAR = 0x09;
const PLAY_DIFFICULTY = 0x0a;
const PLAY_UNLOAD = 0x25;
const PLAY_MAP_CHUNK = 0x2c;
const PLAY_CHUNK_BATCH_START = 0x0c;
const PLAY_CHUNK_BATCH_FINISHED = 0x0b;
const PLAY_LOGIN = 0x30;
const PLAY_KEEP_ALIVE = 0x2b;
const PLAY_KEEP_ALIVE_SERVERBOUND = 0x1b;
const CONFIG_KEEP_ALIVE = 0x04;
const PLAY_KICK = 0x20;
const PLAY_START_CONFIGURATION = 0x74;
const PLAY_COOKIE = 0x15;
const PLAY_PING = 0x3b;
const PLAY_TRANSFER = 0x7f;
const PLAY_ADD_PACK = 0x4f;
const PLAY_REMOVE_PACK = 0x4e;
const PLAY_POSITION = 0x46;
const PLAY_ABILITIES = 0x3e;
const PLAY_GAME_STATE = 0x26;
const PLAY_VIEW_POS = 0x5c;
const PLAY_VIEW_DIST = 0x5d;
const PLAY_SPAWN_POS = 0x5f;
const PLAY_HELD = 0x67;
const PLAY_HEALTH = 0x66;
const PLAY_XP = 0x65;
const PLAY_TIME = 0x6f;
const PLAY_PLAYER_INFO = 0x44;
const PLAY_PLAYER_REMOVE = 0x43;
const PLAY_SCORE_DISPLAY = 0x60;
const PLAY_SCORE_OBJECTIVE = 0x68;
const PLAY_SCORE_SCORE = 0x6c;
const PLAY_RESET_SCORE = 0x4d;
const PLAYER_ENTITY_TYPE = 155;
const PLAY_SYSTEM_CHAT = 0x77;
const PLAY_PLAYER_CHAT = 0x3f;
const PLAY_PROFILELESS_CHAT = 0x21;
const PLAY_ACTION_BAR = 0x55;
const PLAY_TAB_HEADER = 0x78;
const PLAY_TEAMS = 0x6b;
const PLAY_ENTITY_META = 0x61;
const PLAY_ENTITY_EQUIP = 0x64;
const PLAY_ENTITY_DESTROY = 0x4b;
const PLAY_SERVER_DATA = 0x54;
const PLAY_BORDER = 0x2a;
const PLAY_MAP = 0x31;

const SKIP_LIVE = new Set([
    PLAY_LOGIN,
    PLAY_KEEP_ALIVE,
    PLAY_KICK,
    PLAY_START_CONFIGURATION,
    PLAY_COOKIE,
    PLAY_PING,
    PLAY_TRANSFER,
    PLAY_ADD_PACK,
    PLAY_REMOVE_PACK,
]);

export function readVarInt(buf, offset = 0) {
    let value = 0;
    let shift = 0;
    let pos = offset;
    while (pos < buf.length) {
        const b = buf[pos++];
        value |= (b & 0x7f) << shift;
        if ((b & 0x80) === 0) return { value, size: pos - offset };
        shift += 7;
        if (shift > 35) break;
    }
    return null;
}

export function packetId(buf) {
    const v = readVarInt(buf, 0);
    return v ? v.value : null;
}

export function afterPacketId(buf) {
    return readVarInt(buf, 0);
}

function writeVarInt(n) {
    const bytes = [];
    let v = n >>> 0;
    while (true) {
        if ((v & ~0x7f) === 0) {
            bytes.push(v);
            break;
        }
        bytes.push((v & 0x7f) | 0x80);
        v >>>= 7;
    }
    return Buffer.from(bytes);
}

/** FunTime keep_alive is i64; after configuration nmp often never emits the packet, and mineflayer then ends with keepAliveError. */
export function replyKeepAlive(client, state, buf) {
    const hdr = afterPacketId(buf);
    if (!hdr) return false;
    const payload = buf.subarray(hdr.size);
    if (payload.length < 8) return false;
    let replyId = null;
    if (hdr.value === PLAY_KEEP_ALIVE) {
        replyId = PLAY_KEEP_ALIVE_SERVERBOUND;
    } else if (state === 'configuration' && hdr.value === CONFIG_KEEP_ALIVE && payload.length === 8) {
        replyId = CONFIG_KEEP_ALIVE;
    }
    if (replyId == null) return false;
    try {
        client.writeRaw(Buffer.concat([writeVarInt(replyId), payload.subarray(0, 8)]));
        return true;
    } catch {
        return false;
    }
}

export function spawnEntityType(buf) {
    const hdr = afterPacketId(buf);
    if (!hdr) return null;
    const eid = readVarInt(buf, hdr.size);
    if (!eid) return null;
    const uuidOff = hdr.size + eid.size;
    if (buf.length < uuidOff + 16) return null;
    const typ = readVarInt(buf, uuidOff + 16);
    return typ ? typ.value : null;
}

const LIVE_OK = new Set([
    PLAY_MAP_CHUNK,
    PLAY_UNLOAD,
    PLAY_CHUNK_BATCH_START,
    PLAY_CHUNK_BATCH_FINISHED,
    PLAY_POSITION,
    PLAY_BOSS_BAR,
    PLAY_SYSTEM_CHAT,
    PLAY_PLAYER_CHAT,
    PLAY_PROFILELESS_CHAT,
    PLAY_ACTION_BAR,
    PLAY_ABILITIES,
    PLAY_HEALTH,
    PLAY_XP,
    PLAY_TIME,
    PLAY_HELD,
    PLAY_DIFFICULTY,
    PLAY_SPAWN_POS,
    PLAY_GAME_STATE,
    PLAY_VIEW_POS,
    PLAY_VIEW_DIST,
    PLAY_PLAYER_INFO,
    PLAY_PLAYER_REMOVE,
    PLAY_TAB_HEADER,
    PLAY_TEAMS,
    PLAY_SCORE_DISPLAY,
    PLAY_SCORE_OBJECTIVE,
    PLAY_SCORE_SCORE,
    PLAY_RESET_SCORE,
    PLAY_ENTITY_META,
    PLAY_ENTITY_EQUIP,
    PLAY_ENTITY_DESTROY,
    0x33,
    0x34,
    0x36,
    0x51,
    0x63,
    0x7b,
]);

export function shouldForwardLiveWire(state, buf) {
    if (state !== 'play') return false;
    const id = packetId(buf);
    if (id == null) return false;
    if (id === PLAY_SPAWN_ENTITY) return spawnEntityType(buf) === PLAYER_ENTITY_TYPE;
    return LIVE_OK.has(id);
}

export function tapClientPackets(client, onBuf) {
    let attached = null;
    const handler = (buf) => {
        try {
            onBuf(client.state, buf);
        } catch {
            /* ignore */
        }
    };
    const hook = () => {
        // После FunTime transfer/compression новый decompressor появляется не всегда
        // через setCompressionThreshold — цепляемся заново при любом state и по таймеру.
        const src = client.decompressor || client.splitter;
        if (!src) return;
        if (src === attached) return;
        if (attached) attached.removeListener('data', handler);
        attached = src;
        // prepend: отвечаем keep_alive до парсера nmp, иначе event+raw = двойной echo
        if (typeof src.prependListener === 'function') src.prependListener('data', handler);
        else src.on('data', handler);
    };
    hook();
    client.on('state', hook);
    const orig = client.setCompressionThreshold.bind(client);
    client.setCompressionThreshold = (threshold) => {
        const r = orig(threshold);
        // после смены порога decompressor может появиться на следующем тике
        hook();
        setImmediate(hook);
        return r;
    };
    // FunTime /an* → configuration: иногда threshold уже был, а pipe новый
    const rehookTimer = setInterval(hook, 2000);
    rehookTimer.unref?.();
    const stop = () => {
        clearInterval(rehookTimer);
        if (attached) {
            try { attached.removeListener('data', handler); } catch { /* ignore */ }
            attached = null;
        }
    };
    client.on('end', stop);
    client.on('error', stop);
}

export const WIRE_IDS = {
    PLAY_SPAWN_ENTITY,
    PLAY_BOSS_BAR,
    PLAY_DIFFICULTY,
    PLAY_UNLOAD,
    PLAY_MAP_CHUNK,
    PLAY_CHUNK_BATCH_START,
    PLAY_CHUNK_BATCH_FINISHED,
    PLAY_POSITION,
    PLAY_ABILITIES,
    PLAY_GAME_STATE,
    PLAY_VIEW_POS,
    PLAY_VIEW_DIST,
    PLAY_SPAWN_POS,
    PLAY_HELD,
    PLAY_HEALTH,
    PLAY_XP,
    PLAY_TIME,
    PLAY_PLAYER_INFO,
    PLAY_PLAYER_REMOVE,
    PLAY_SCORE_DISPLAY,
    PLAY_SCORE_OBJECTIVE,
    PLAY_SCORE_SCORE,
    PLAY_RESET_SCORE,
    PLAYER_ENTITY_TYPE,
    PLAY_SYSTEM_CHAT,
    PLAY_PLAYER_CHAT,
    PLAY_PROFILELESS_CHAT,
    PLAY_ACTION_BAR,
    PLAY_TAB_HEADER,
    PLAY_TEAMS,
    PLAY_ENTITY_META,
    PLAY_ENTITY_EQUIP,
    PLAY_ENTITY_DESTROY,
    PLAY_SERVER_DATA,
    PLAY_BORDER,
    PLAY_MAP,
};
