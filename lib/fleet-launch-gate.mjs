/**
 * Глобальный gate запуска: Go сериализует логины всего флота.
 * POST /api/fleet/launch → { granted, wait_ms, position }.
 */
import { goHttpBase } from './go-http-base.mjs';

function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
}

function bypassLaunchGate() {
    return process.env.FLEET_LAUNCH_BYPASS === '1'
        || process.env.FLEET_LAUNCH_BYPASS === 'true'
        || process.env.LOCAL_MODE === '1'
        || process.env.LOCAL_MODE === 'true';
}

/**
 * @param {{
 *   username: string,
 *   anarchy: string|number,
 *   kind?: 'bot'|'owner',
 *   log?: (s: string) => void,
 *   signal?: { aborted?: boolean },
 * }} opts
 */
export async function awaitFleetLaunchGrant({
    username,
    anarchy,
    kind = 'bot',
    log = console.log,
    signal = null,
}) {
    const nick = String(username || '').trim();
    const an = Number(String(anarchy ?? '').replace(/\D/g, '').slice(0, 3)) || 0;
    if (!nick || !an) {
        log(`[launch-gate] skip bad nick/an nick=${nick} an=${an}`);
        return false;
    }
    if (bypassLaunchGate()) {
        log(`[launch-gate] BYPASS ${kind} ${nick} an${an} (LOCAL_MODE/FLEET_LAUNCH_BYPASS)`);
        return true;
    }

    const url = `${goHttpBase()}/api/fleet/launch`;
    let failures = 0;
    for (;;) {
        if (signal?.aborted) {
            log(`[launch-gate] aborted ${nick}`);
            return false;
        }
        try {
            const res = await fetch(url, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ anarchy: an, username: nick, kind }),
            });
            const data = await res.json().catch(() => ({}));
            if (!res.ok || data?.ok === false) {
                failures++;
                const wait = Math.min(30_000, 3000 * failures);
                log(`[launch-gate] HTTP ${res.status} ${nick} — retry ${wait / 1000}с (${data?.reason || ''})`);
                await sleep(wait);
                continue;
            }
            failures = 0;
            if (data.granted) {
                log(
                    `[launch-gate] GRANT ${kind} ${nick} an${an}`
                    + (data.next_gap_s ? ` (next gap ~${data.next_gap_s}с)` : ''),
                );
                return true;
            }
            let wait = Number(data.wait_ms) || 5000;
            if (wait < 1000) wait = 1000;
            if (wait > 120_000) wait = 120_000;
            const pos = data.position ? ` pos=${data.position}` : '';
            log(`[launch-gate] wait ${kind} ${nick}${(pos)} ${ (wait / 1000).toFixed(0)}с (${data.reason || '…'})`);
            await sleep(wait);
        } catch (e) {
            failures++;
            const wait = Math.min(45_000, 5000 * failures);
            log(`[launch-gate] Go недоступен (${e.message}) — ${nick} ждём ${wait / 1000}с (fail-closed)`);
            await sleep(wait);
        }
    }
}

export async function cancelFleetLaunch({ username, anarchy, log = console.log }) {
    if (bypassLaunchGate()) return;
    const nick = String(username || '').trim();
    const an = Number(String(anarchy ?? '').replace(/\D/g, '').slice(0, 3)) || 0;
    if (!nick || !an) return;
    try {
        await fetch(`${goHttpBase()}/api/fleet/launch`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ anarchy: an, username: nick, cancel: true }),
        });
    } catch (e) {
        log(`[launch-gate] cancel fail ${nick}: ${e.message}`);
    }
}
