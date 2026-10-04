/**
 * Прокси владельцев кланов — отдельно от ip.json (боты).
 */
import { readFileSync, existsSync } from 'fs';
import { join } from 'path';

function loadJson(path) {
    return JSON.parse(readFileSync(path, 'utf8'));
}

/**
 * @param {string} rootDir
 * @param {{ ip?: string, ownerIp?: string, anarchy?: string|number }} owner
 * @param {string|number} [anarchy]
 * @returns {string|null}
 */
export function resolveOwnerProxyString(rootDir, owner, anarchy) {
    const an = String(anarchy ?? owner?.anarchy ?? '').replace(/\D/g, '').slice(0, 3);
    const key = String(owner?.ownerIp || owner?.ip || an || '').trim();
    if (!key) return null;

    const ownerIpPath = join(rootDir, 'owner-ip.json');
    if (!existsSync(ownerIpPath)) {
        return null;
    }
    const ownerIp = loadJson(ownerIpPath);
    const proxy = ownerIp[key];
    if (typeof proxy === 'string' && proxy.trim()) {
        return proxy.trim();
    }
    return null;
}

/**
 * @param {string} rootDir
 * @param {string|number} anarchy
 * @returns {{ owner: object, proxyString: string }|null}
 */
export function loadClanOwnerSession(rootDir, anarchy) {
    const an = String(anarchy).replace(/\D/g, '').slice(0, 3);
    const ownersPath = join(rootDir, 'clan-owners.json');
    if (!existsSync(ownersPath)) return null;

    const owners = loadJson(ownersPath);
    const owner = owners[an];
    if (!owner?.username || !owner?.password) return null;

    const proxyString = resolveOwnerProxyString(rootDir, owner, an);
    if (!proxyString) return null;

    return { owner, proxyString };
}
