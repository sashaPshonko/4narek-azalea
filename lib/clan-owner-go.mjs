/** Статус владельца клана на Go /fleet (без WebSocket оркестратора). */
const GO_HTTP = process.env.GO_HTTP_URL
    || (process.env.LOCAL_MODE === '1' || process.env.LOCAL_MODE === 'true'
        ? 'http://127.0.0.1:8080'
        : 'http://212.8.229.76:8080');

/**
 * @param {{
 *   username: string,
 *   anarchy: string|number,
 *   status?: 'ok'|'banned'|'error'|'pending',
 *   banned?: boolean,
 *   reason?: string,
 *   bannedAt?: string|null,
 *   checkedAt?: string|null,
 *   ip?: string,
 * }} opts
 */
export function reportClanOwnerToGo(opts) {
    const username = String(opts?.username || '').trim();
    if (!username) return;
    const status = opts.banned || opts.status === 'banned' ? 'banned' : (opts.status || 'ok');
    const body = {
        username,
        anarchy: opts.anarchy ?? null,
        status,
        banned: status === 'banned',
        banned_at: opts.bannedAt || null,
        reason: opts.reason || '',
        checked_at: opts.checkedAt || new Date().toISOString(),
        ip: opts.ip || '',
    };
    fetch(`${GO_HTTP}/api/clan-owner`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    }).catch((e) => console.warn(`[clan-owner] go report: ${e.message}`));
}
