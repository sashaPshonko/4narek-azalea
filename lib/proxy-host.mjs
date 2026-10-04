/** Хост прокси без порта: socks5://user:pass@1.2.3.4:50101 → 1.2.3.4 */
export function proxyHostFromString(s) {
    const raw = String(s || '').trim();
    if (!raw) return '';
    try {
        const withScheme = raw.includes('://') ? raw : `socks5://${raw}`;
        const u = new URL(withScheme);
        if (u.hostname) return u.hostname;
    } catch { /* fall through */ }
    const at = raw.lastIndexOf('@');
    let hostport = at >= 0 ? raw.slice(at + 1) : raw.replace(/^socks5:\/\//i, '');
    hostport = hostport.split('/')[0];
    const colon = hostport.lastIndexOf(':');
    if (colon > 0 && /^\d+$/.test(hostport.slice(colon + 1))) {
        return hostport.slice(0, colon);
    }
    return hostport;
}
