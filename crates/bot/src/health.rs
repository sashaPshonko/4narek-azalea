//! Живость воркера, как в JS-версии (`lib/funtime-keepalive.mjs`, `4NAREK.mjs`).
//!
//! 1. **Мёртвый сокет.** Если пропал интернет, TCP остаётся «полуоткрытым»: ни пакетов, ни ошибки, `Disconnect`
//!    не приходит, а АХ-цикл пишет команды в пустоту. Как в JS (0 входящих за 90с → `client.end`), смотрим на
//!    сокет, а не на игру: azalea (vendor) пишет время последнего принятого с сокета пакета в любом состоянии
//!    (логин / конфигурация / игра). Тишина дольше `IDLE_END` — воркер мёртв: `worker_dead`, закрываем
//!    соединение и выходим, оркестратор поднимет чистую сессию.
//! 2. **Прокси.** Как в JS: любая ошибка подключения через SOCKS-прокси — сразу `proxy_error` (`auth_fault` +
//!    выход 72). Оркестратор отдаёт это в Go (`auth_faults` с ip прокси), Go группирует по прокси (`proxy_faults`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ipc::{emit, WorkerMsg};
use serde_json::json;
use tracing::warn;

/// Сколько сокет может молчать (JS: 90с). Сервер шлёт keepalive раз в ~15с.
const IDLE_END: Duration = Duration::from_secs(90);

/// Код выхода при неисправном прокси (оркестратор помечает `proxy_error`).
pub const EXIT_PROXY: i32 = 72;

static DEAD: AtomicBool = AtomicBool::new(false);
static PROXY_OK_SENT: AtomicBool = AtomicBool::new(false);

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Воркер объявлен мёртвым: команды в чат не шлём.
pub fn is_dead() -> bool {
    DEAD.load(Ordering::SeqCst)
}

/// Явная смерть воркера: сообщаем оркестратору, рвём соединение и выходим.
fn die(reason: &str) {
    if DEAD.swap(true, Ordering::SeqCst) {
        return;
    }
    let g = crate::boot::global();
    warn!("воркер мёртв: {reason} → exit 1");
    {
        let mut st = g.state.lock();
        st.on_anarchy = false;
        st.shutdown = true;
    }
    emit(&WorkerMsg {
        name: "worker_dead".into(),
        username: Some(g.boot.username.clone()),
        reason: Some(reason.to_string()),
        ..crate::empty_msg()
    });
    if let Some(bot) = g.bot_client.lock().clone() {
        let _ = bot.disconnect();
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(400));
        std::process::exit(1);
    });
}

/// Фоновая проверка сокета. Запускать один раз при старте процесса.
pub fn spawn_watchdog() {
    let started = unix_ms();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));
        if is_dead() {
            return;
        }
        // Пока соединения ещё не было (старт), считаем тишину от запуска процесса.
        let last = azalea_client::connection::last_wire_rx_ms().max(started);
        let idle = unix_ms().saturating_sub(last);
        if idle > IDLE_END.as_millis() as u64 {
            die(&format!("сокет молчит {}с", idle / 1000));
            return;
        }
    });
}

/// Бот заспавнился — значит, соединение через прокси работает. Один раз за процесс сообщаем оркестратору
/// (`proxy_ok`), чтобы он снял пометку `proxy_error`, если прокси ломался раньше.
pub fn on_spawn() {
    if PROXY_OK_SENT.swap(true, Ordering::SeqCst) {
        return;
    }
    let g = crate::boot::global();
    emit(&WorkerMsg {
        name: "proxy_ok".into(),
        username: Some(g.boot.username.clone()),
        ..crate::empty_msg()
    });
}

/// azalea не смогла создать соединение. Через SOCKS-прокси это ошибка прокси (как в JS): сразу
/// `proxy_error` и выход 72. Без прокси ("direct") — просто лог, azalea повторит попытку сама.
pub fn on_connection_failed(error: String) {
    let g = crate::boot::global();
    warn!("подключение не удалось: {error}");
    if g.boot.proxy.trim().is_empty() || g.boot.proxy.trim() == "direct" {
        return;
    }
    if DEAD.swap(true, Ordering::SeqCst) {
        return;
    }
    emit(&WorkerMsg {
        name: "auth_fault".into(),
        username: Some(g.boot.username.clone()),
        reason: Some(error.clone()),
        data: Some(json!({ "kind": "proxy_error" })),
        ..crate::empty_msg()
    });
    // exit сразу после emit может потерять сообщение — как в JS, даём 400мс.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(400));
        std::process::exit(EXIT_PROXY);
    });
}
