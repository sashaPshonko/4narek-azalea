/** Общий base URL Go HTTP (оркестраторы / скрипты). */
export function goHttpBase() {
    return process.env.GO_HTTP_URL
        || (process.env.LOCAL_MODE === '1' || process.env.LOCAL_MODE === 'true'
            ? 'http://127.0.0.1:8080'
            : 'http://212.8.229.76:8080');
}
