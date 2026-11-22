//! Orch на Rust: process-per-bot (как 502b.mjs).
//! Конфиг: `orch.json`. Боты/прокси: `farm_root/bots/{an}b.json`, `ip.json`.
//! `myNick` берётся из `mynick.json` (`username`), отдельного овнера нет.
//!
//! **Казначей** — роль на farm-боте. Орк раз в 30с вычисляет казначея:
//! farm-бот с `is_active` и **max `next_break_at`** (дольше всех до сна),
//! причём до сна должно быть **больше `TREASURER_MIN_RUNUP`** — иначе
//! казначей сменится через 2 минуты и начнётся дёрготня flush-циклов.
//! Шлёт всем `OrchCmd::TreasurerRole { username }` при смене.
//! Казначею шлёт `OrchCmd::BalancesSnapshot { balances }` раз в 30с
//! (чтобы тот раздавал излишек в `sell_pass`).
//!
//! **Перед сном казначея** (по schedule) — орк НЕ убивает его, пока тот
//! не отдаст всё `bal - KEEP_SUM` новому казначею. Шлёт `FlushToTreasurer`,
//! ждёт `WorkerMsg { name: "treasurer_flushed" }` **без таймаута**.
//! Пока ждёт — `schedule tick` пропускает `kill_worker` для этого бота.
//! Если `treasurer_flushed` так и не пришёл (бот завис) — орк **продолжает
//! ждать** (по твоему требованию «без таймаута»). Но если процесс умер —
//! орк считает, что flush провалился, и запоминает `owes_treasurer`
//! в `OrchState`, чтобы после респавна дослать.
//!
//! **`owes_treasurer`** — источник истины «этот бот должен отдать деньги
//! вот этому казначею». Ставится при смене казначея, при смерти воркера
//! во время flush, сохраняется при `treasurer_flushed { ok: false }`.
//! Снимается при `ok: true` или при досылке `FlushToTreasurer`.
//!
//! **`flush_waiting`** — множество ников, от которых ждём `treasurer_flushed`.
//! `schedule tick` не убивает такого бота.
//!
//! **`bindings.json`** (`farm_root/bindings.json`) — **системный** файл.
//! Пользователь его **не трогает**. Формат: `{ "nick": true/false }`.
//!   - **Нет записи** — не знаем, привязан ли. **Спавним на 5с** (проверка).
//!     В перерыве **НЕ убиваем** (перерыва нет, пока не узнаем).
//!   - **`true`** — привязан. В перерыве убиваем (отдыхает по расписанию).
//!   - **`false`** — не привязан. В перерыве **НЕ убиваем** (перерыва нет).
//! После 5с проверки:
//!   - `success = true` и нет `funauth_bind`/`funauth_2fa`/`auth_fault` → `true`;
//!   - иначе → `false`.
//! При `funauth_bind`/`funauth_2fa`/`auth_fault` — сразу пишем `false`
//! (привязки нет, перерыва нет).

mod config;
mod listing_store;
mod mynick;
mod schedule;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use ipc::{
    pretty_log::{self, LineKind},
    BotBoot, OrchCmd, WorkerMsg,
};
use schedule::{Schedule, ScheduleConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ============ farm-боты ============

#[derive(Debug, Clone, Deserialize)]
struct BotJson {
    username: String,
    password: String,
    anarchy: u16,
    #[serde(default)]
    item: String,
    #[serde(rename = "goType")]
    go_type: String,
    /// Прокси прямо в конфиге бота (`socks5://user:pass@host:port`); приоритетнее `ip`.
    #[serde(default)]
    proxy: String,
    /// Устаревший слот из `ip.json` (используется, только если `proxy` пуст).
    #[serde(default)]
    ip: String,
}

#[derive(Debug)]
struct BotRuntime {
    cfg: BotJson,
    proxy: String,
    success: bool,
    on_anarchy: bool,
    banned: bool,
    manual_stop: bool,
    unstable_until: Option<std::time::Instant>,
    pricing_edges: std::collections::VecDeque<std::time::Instant>,
    last_pricing_ok: bool,
    items: Value,
    inventory: Value,
    balance: Option<f64>,
    balance_at: Option<std::time::Instant>,
    banned_at: Option<String>,
    banned_reason: Option<String>,
    auth_fault: Option<String>,
    auth_fault_at: Option<String>,
    auth_fault_reason: Option<String>,
    spawning: bool,
    /// До какого момента у бота арендованы дополнительные слоты АХ (unix-мс, 0 = не арендованы).
    /// Бот присылает сообщение `rent` при старте (из файла) и после аренды.
    rent_until_ms: u64,
}

/// Слотов АХ у бота без аренды / с арендой (держать в синхроне с `bot/src/rent.rs`).
const BASE_SLOTS: u32 = 5;
const RENTED_SLOTS: u32 = 10;

/// Сколько слотов АХ у бота в момент `now_ms`: 10, пока аренда действует, иначе 5.
fn slots_for(rent_until_ms: u64, now_ms: u64) -> u32 {
    if rent_until_ms > now_ms {
        RENTED_SLOTS
    } else {
        BASE_SLOTS
    }
}

// ============ workers ============

struct WorkerHandle {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Debug, Clone)]
struct PendingPay {
    to_bot: String,
    amount: i64,
    at: std::time::Instant,
}

// ============ state ============

struct OrchState {
    bots: HashMap<String, BotRuntime>,
    my_nick: Option<String>,
    workers: HashMap<String, WorkerHandle>,
    bot_bin: PathBuf,
    bots_path: PathBuf,
    shutting_down: bool,
    items_buying: Value,
    listings: listing_store::ListingStore,
    last_catalog: Vec<Value>,
    pending_pays: HashMap<u64, PendingPay>,
    next_req_id: u64,

    restart_tx: mpsc::UnboundedSender<String>,

    schedule: Arc<Schedule>,
    bot_order: Vec<String>,

    // ============================================================
    // BINDINGS (funauth привязка) — системный файл
    // ============================================================

    /// Ник → привязан? Источник истины — `bindings.json`.
    /// - **нет записи** — не знаем (спавним на 5с проверить).
    /// - `true` — привязан (в перерыве убиваем).
    /// - `false` — не привязан (в перерыве НЕ убиваем).
    bindings: HashMap<String, bool>,
    bindings_path: PathBuf,

    // ============================================================
    // КАЗНАЧЕЙ
    // ============================================================

    /// Текущий казначей (nick). `None` — ещё не определён.
    treasurer_nick: Option<String>,

    /// Кто-то должен отдать излишек казначею — но не смог. После
    /// респавна `spawn_worker_inner` проверит и дошлёт `FlushToTreasurer`.
    owes_treasurer: HashMap<String, String>,

    /// `username` → ждём `treasurer_flushed` от него (flush в полёте).
    flush_waiting: std::collections::HashSet<String>,

    /// Разбуженные досрочно для приёма денег: nick → конец их планового сна.
    early_wake: HashMap<String, u64>,
    /// Воркеры, которым отправлен Park{on:true} (стоят в хабе на перерыве).
    parked: std::collections::HashSet<String>,
}

type GoTx = mpsc::UnboundedSender<Value>;
type RestartTx = mpsc::UnboundedSender<String>;

const MIN_BOT_BALANCE_BASE: f64 = 13_000_000.0;
fn min_bot_balance() -> f64 { ipc::balance::k(MIN_BOT_BALANCE_BASE) }
const HIGH_BALANCE_TRIGGER_BASE: f64 = 30_000_000.0;
fn high_balance_trigger() -> f64 { ipc::balance::k(HIGH_BALANCE_TRIGGER_BASE) }
/// Сентинел «баланс неизвестен» — совпадает с owner::BALANCE_UNKNOWN.
const BALANCE_UNKNOWN: f64 = -1.0;
/// Как часто пересчитываем казначея.
const TREASURER_TICK: Duration = Duration::from_secs(30);
/// После перевода между ботами оба участника ждут, пока придёт свежий баланс (его присылает каждая продажа
/// и сам донор сразу после `/pay`). Это только запасной предел на случай, если баланс так и не пришёл.
const PEER_PAY_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// Баланс, пришедший раньше этого после приказа, перевод ещё не учитывает.
const PEER_PAY_SETTLE: Duration = Duration::from_secs(20);
/// После «слива» уходящего бота казначею тот же бот повторно не сливается столько.
const DUMP_COOLDOWN: Duration = Duration::from_secs(150);
/// Сколько бот в `flush_waiting` исключается из выравнивания; потом считаем flush зависшим.
const FLUSH_BLOCK_MAX: Duration = Duration::from_secs(3 * 60);
/// Не больше стольких переводов выравнивания в час на анархию (чтобы не светить частые /pay).
const MAX_PEER_PAYS_PER_HOUR: usize = 15;
/// Если до конца смены меньше — бот «уходящий»: ему не кидают, он не участвует в выравнивании.
const LEAVING_SOON_MS: u64 = 10 * 60 * 1000;

/// Уходящие в перерыв боты отдают весь баланс казначею, но только если у казначея меньше 13кк.
fn plan_dump(
    leavers: &[(String, f64)],
    treasurer: Option<&(String, f64)>,
) -> Vec<(String, String, i64)> {
    let Some((t, tb)) = treasurer else { return vec![] };
    if *tb < 0.0 || *tb >= poor_below() {
        return vec![];
    }
    leavers
        .iter()
        .filter(|(n, b)| n != t && *b >= DUMP_MIN_PAY)
        .map(|(n, b)| (n.clone(), t.clone(), b.floor() as i64))
        .collect()
}

/// Бот «беден», если у него меньше 13кк (тот же порог, что у раздачи казначея).
const POOR_BELOW_BASE: f64 = 13_000_000.0;
fn poor_below() -> f64 { ipc::balance::k(POOR_BELOW_BASE) }
/// Мельче `/pay` не шлём (палево) — как в боте.
const DUMP_MIN_PAY: f64 = 200_000.0;
/// Выравнивание двигает деньги только крупными суммами: мелкие недобор/перебор не стоят `/pay`.
const PEER_MIN_PAY: f64 = 1_000_000.0;
/// Потолок «среднего»: больше 15кк на фарм-боте не выравниваем (излишек уходит казначею).
const PEER_FAIR_CAP_BASE: f64 = 15_000_000.0;
fn peer_fair_cap() -> f64 { ipc::balance::k(PEER_FAIR_CAP_BASE) }

/// Выравнивание: кто беднее 13кк — получает до среднего (не выше 15кк)
/// от тех, у кого выше среднего. Идёт, пока у всех не станет ≥13кк. Возвращает (от кого, кому, сумма).
fn plan_rebalance(bals: &[(String, f64)]) -> Vec<(String, String, i64)> {
    let known: Vec<&(String, f64)> = bals.iter().filter(|(_, b)| *b >= 0.0).collect();
    if known.len() < 2 {
        return vec![];
    }
    // выше 15кк у бота не держим — это излишек казначея
    let fair = (known.iter().map(|(_, b)| *b).sum::<f64>() / known.len() as f64).min(peer_fair_cap());
    let mut poor: Vec<(String, f64)> = known
        .iter()
        .filter(|(_, b)| *b < poor_below() && fair - *b >= PEER_MIN_PAY)
        .map(|(n, b)| (n.clone(), fair - *b))
        .collect();
    poor.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut rich: Vec<(String, f64)> = known
        .iter()
        .filter(|(_, b)| *b - fair >= PEER_MIN_PAY)
        .map(|(n, b)| (n.clone(), *b - fair))
        .collect();
    rich.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    // Один перевод на донора за тик (бот держит один приказ). Получателю стараемся отдать всё одним донором
    // (наименьшим из тех, кому хватает). Если никто один не покрывает — добираем нескольких доноров подряд,
    // начиная с самых богатых, чтобы недостачу закрыть за один тик, а не за несколько кругов с паузами.
    let mut used = vec![false; rich.len()];
    for (name, need) in poor.iter() {
        let fit = rich
            .iter()
            .enumerate()
            .filter(|(i, (_, sur))| !used[*i] && *sur >= *need)
            .min_by(|a, b| a.1 .1.partial_cmp(&b.1 .1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i);
        if let Some(i) = fit {
            let amount = need.floor();
            if amount >= PEER_MIN_PAY {
                used[i] = true;
                out.push((rich[i].0.clone(), name.clone(), amount as i64));
            }
            continue;
        }
        let mut order: Vec<usize> = (0..rich.len()).filter(|i| !used[*i]).collect();
        order.sort_by(|a, b| rich[*b].1.partial_cmp(&rich[*a].1).unwrap_or(std::cmp::Ordering::Equal));
        let mut left = *need;
        for i in order {
            if left < PEER_MIN_PAY {
                break;
            }
            let amount = rich[i].1.min(left).floor();
            if amount < PEER_MIN_PAY {
                continue;
            }
            used[i] = true;
            left -= amount;
            out.push((rich[i].0.clone(), name.clone(), amount as i64));
        }
    }
    out
}

#[cfg(test)]
mod rebalance_tests {
    use super::*;
    fn v(a: &[(&str, f64)]) -> Vec<(String, f64)> {
        a.iter().map(|(n, b)| (n.to_string(), *b)).collect()
    }
    fn plan_rebalance_t(b: &[(String, f64)]) -> Vec<(String, String, i64)> {
        plan_rebalance(b)
    }
    #[test]
    fn rich_treasurer_equalizes_like_a_normal_bot() {
        let r = plan_rebalance(&v(&[("t", 20e6), ("a", 3e6), ("b", 3e6)]));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "t");
        assert_eq!(r[0].2, 5_666_666);
    }
    #[test]
    fn leaver_dumps_everything_to_poor_treasurer() {
        let t = ("t".to_string(), 5e6);
        let r = plan_dump(&v(&[("a", 7_500_000.5), ("b", 100_000.0)]), Some(&t));
        assert_eq!(r, vec![("a".into(), "t".into(), 7_500_000)]);
    }
    #[test]
    fn leaver_keeps_money_if_treasurer_has_13m() {
        let t = ("t".to_string(), 13e6);
        assert!(plan_dump(&v(&[("a", 7e6)]), Some(&t)).is_empty());
        assert!(plan_dump(&v(&[("a", 7e6)]), None).is_empty());
    }
    #[test]
    fn one_donor_covers_whole_need() {
        // 24 / 12 / 0 -> среднее 12; недостача c = 12кк; покрыть может только a (избыток 12кк)
        let r = plan_rebalance_t(&v(&[("a", 24e6), ("b", 12e6), ("c", 0.0)]));
        assert_eq!(r, vec![("a".into(), "c".into(), 12_000_000)]);
    }
    #[test]
    fn small_gaps_are_not_worth_a_pay() {
        // недостача 0,6кк < 1кк
        assert!(plan_rebalance_t(&v(&[("a", 8.6e6), ("b", 8.0e6)])).is_empty());
    }
    #[test]
    fn each_poor_gets_one_transfer() {
        let r = plan_rebalance_t(&v(&[("a", 30e6), ("b", 20e6), ("c", 0.0), ("d", 1e6)]));
        assert_eq!(r.len(), 2);
        assert!(r.iter().any(|x| x.1 == "c") && r.iter().any(|x| x.1 == "d"));
    }
    #[test]
    fn six_and_zero_split_three_each() {
        let b = v(&[("t", 0.0), ("a", 6e6)]);
        assert_eq!(plan_rebalance(&b), vec![("a".into(), "t".into(), 3_000_000)]);
    }
    #[test]
    fn stops_when_everyone_has_13m() {
        assert!(plan_rebalance(&v(&[("a", 13e6), ("b", 14e6), ("c", 30e6)])).is_empty());
    }
    #[test]
    fn rich_pays_poor_up_to_average() {
        let r = plan_rebalance_t(&v(&[("a", 11_200_000.0), ("b", 0.0)]));
        assert_eq!(r, vec![("a".into(), "b".into(), 5_600_000)]);
    }
    #[test]
    fn fair_is_capped_at_keep_sum() {
        let r = plan_rebalance_t(&v(&[("a", 40e6), ("b", 0.0)]));
        assert_eq!(r, vec![("a".into(), "b".into(), 15_000_000)]);
    }
    #[test]
    fn balanced_bots_do_nothing() {
        assert!(plan_rebalance_t(&v(&[("a", 8.2e6), ("b", 8.1e6), ("c", 8.0e6)])).is_empty());
    }
    #[test]
    fn unknown_and_single_ignored() {
        assert!(plan_rebalance_t(&v(&[("a", 10e6), ("b", -1.0)])).is_empty());
    }
    #[test]
    fn several_rich_split_one_poor_need() {
        // никто один не покрывает недостачу c (6кк): берём обоих доноров, недостача закрывается за один тик
        let r = plan_rebalance_t(&v(&[("a", 9e6), ("b", 9e6), ("c", 0.0)]));
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.1 == "c" && x.2 == 3_000_000));
        assert_ne!(r[0].0, r[1].0);
    }
}
/// Казначеем не может стать бот, которому до сна меньше этого.
const TREASURER_MIN_RUNUP_MS: u64 = 2 * 60 * 1000;
/// Максимальная задержка начала длинного сна, если нет замены.
const MAX_SLEEP_DEFER_MS: u64 = 90 * 60 * 1000;
// `BIND_CHECK_SECS` больше не используется — `bindings = true`
// ставит `funauth_verified` (воркер уже ждёт 5с от `success`).
#[tokio::main]
async fn main() -> Result<()> {
    pretty_log::init_orch();

    let loaded = config::load()?;
    let farm_root = loaded.farm_root.clone();
    let anarchy = loaded.cfg.anarchy;
    ipc::balance::init(anarchy);
    let ws_url = loaded.cfg.go_ws.clone();
    let bot_bin = config::bot_bin_path()?;
    let mynick_bin = bot_bin.clone();
    info!("bot={}", bot_bin.display());

    let bots_path = farm_root.join(format!("bots/{anarchy}b.json"));
    let ip_path = farm_root.join("ip.json");
    let (bots, bot_order) = load_bots(&bots_path, &ip_path)?;

    let bots_filename = format!("{anarchy}b");
    let schedule_cfg = ScheduleConfig::load(&farm_root);
    let schedule = Arc::new(Schedule::new(
        anarchy,
        &bots_filename,
        bot_order.len(),
        &schedule_cfg,
    ));
    info!(
        "schedule: an{anarchy} ({bots_filename}) — {} ботов",
        bot_order.len()
    );

    let my_nick = load_my_nick(&farm_root)?;
    info!("myNick={:?}", my_nick);

    let listings = listing_store::ListingStore::new(listing_store::default_dir(&farm_root));

    // ============ bindings.json ============
    let bindings_path = farm_root.join("bindings.json");
    let bindings = load_bindings(&bindings_path);
    kill_orphan_workers(anarchy);
    info!(
        "bindings.json: {} записей (привязано: {})",
        bindings.len(),
        bindings.values().filter(|v| **v).count()
    );

    let (go_tx, go_rx) = mpsc::unbounded_channel::<Value>();
    let (restart_tx, mut restart_rx) = mpsc::unbounded_channel::<String>();

    let state = Arc::new(Mutex::new(OrchState {
        bots,
        my_nick,
        workers: HashMap::new(),
        bot_bin,
        bots_path,
        shutting_down: false,
        items_buying: json!([]),
        listings,
        last_catalog: Vec::new(),
        pending_pays: HashMap::new(),
        next_req_id: 0,

        restart_tx: restart_tx.clone(),

        schedule: schedule.clone(),
        bot_order,

        bindings,
        bindings_path,

        treasurer_nick: None,
        owes_treasurer: HashMap::new(),
        flush_waiting: std::collections::HashSet::new(),
        early_wake: HashMap::new(),
        parked: std::collections::HashSet::new(),
    }));

    tokio::spawn(go_ws_loop(state.clone(), go_tx.clone(), go_rx, ws_url));

    // Рестарт farm-ботов.
    let st_r = state.clone();
    let go_r = go_tx.clone();
    let restart_tx_loop = restart_tx.clone();
    tokio::spawn(async move {
        while let Some(name) = restart_rx.recv().await {
            sleep(Duration::from_secs(15)).await;
            let already = {
                let g = st_r.lock().await;
                g.workers.contains_key(&name)
            };
            if already {
                info!("рестарт {name} — уже жив, не трогаю");
                continue;
            }
            info!("рестарт {name}");
            if let Err(e) =
                spawn_worker(st_r.clone(), go_r.clone(), restart_tx_loop.clone(), name.clone())
                    .await
            {
                error!("restart {name}: {e:#}");
            }
        }
    });

    // TTL pending_pays (120с) — раз в 30с
    let st_ttl = state.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(30)).await;
            let mut g = st_ttl.lock().await;
            let now = std::time::Instant::now();
            let before = g.pending_pays.len();
            g.pending_pays
                .retain(|_, p| now.duration_since(p.at) < Duration::from_secs(120));
            let removed = before - g.pending_pays.len();
            if removed > 0 {
                tracing::debug!("pending_pays: TTL вычистил {removed}");
            }
        }
    });

    sleep(Duration::from_secs(2)).await;
    start_all(state.clone(), go_tx.clone(), restart_tx.clone()).await;

    let st2 = state.clone();
    let go2 = go_tx.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(30)).await;
            let _ = push_presence(&st2, &go2).await;
        }
    });

    // ============ тикер расписания перерывов ============
    let st_sched = state.clone();
    let go_sched = go_tx.clone();
    let restart_sched = restart_tx.clone();
    tokio::spawn(async move {
        sleep(Duration::from_secs(30)).await;
        loop {
            let (order, schedule_arc, alive, ready, an, flush_waiting, treasurer, bindings) = {
                let g = st_sched.lock().await;
                let an = g.bots.values().next().map(|b| b.cfg.anarchy).unwrap_or(0);
                let alive: std::collections::HashSet<String> =
                    g.workers.keys().cloned().collect();
                // «Готов принять деньги»: живой и не спит сейчас.
                let now0 = schedule::now_unix_ms();
                let ready: std::collections::HashSet<String> = g
                    .bot_order
                    .iter()
                    .enumerate()
                    .filter(|(i, n)| {
                        g.workers.contains_key(*n) && !g.schedule.is_in_long_break(*i, now0)
                    })
                    .map(|(_, n)| n.clone())
                    .collect();
                (
                    g.bot_order.clone(),
                    g.schedule.clone(),
                    alive,
                    ready,
                    an,
                    g.flush_waiting.clone(),
                    g.treasurer_nick.clone(),
                    g.bindings.clone(),
                )
            };
            let now = schedule::now_unix_ms();
            for idx in 0..order.len() {
                info!("schedule an{an}: {}", schedule_arc.debug_state(idx, now));
            }
            let active_vec: Vec<_> = (0..order.len())
                .map(|i| (i, schedule_arc.is_active(i, now)))
                .collect();
            let n_active = active_vec.iter().filter(|(_, a)| *a).count();
            info!(
                "schedule tick an{an}: active={} / {} — {:?}",
                n_active,
                order.len(),
                active_vec
            );
            for (idx, name) in order.iter().enumerate() {
                let active = schedule_arc.is_active(idx, now);
                let running = alive.contains(name);
                let binding = bindings.get(name).copied();
                // Есть привязка? `None` = не знаем.
                let is_bound = binding == Some(true);
                // Знаем, что привязан? Если `None` — надо проверить.
                let needs_check = binding.is_none() || !is_bound;

                if active && running {
                    let was_parked = st_sched.lock().await.parked.remove(name);
                    if was_parked {
                        info!("schedule an{an}: {name} (idx={idx}) выходит из хаба на анку");
                        let _ = send_cmd_to(&st_sched, name, &OrchCmd::Park { on: false }).await;
                    }
                } else if active && !running {
                    info!("schedule an{an}: {name} (idx={idx}) возвращается");
                    if let Err(e) = spawn_worker(
                        st_sched.clone(),
                        go_sched.clone(),
                        restart_sched.clone(),
                        name.clone(),
                    )
                    .await
                    {
                        error!("schedule spawn {name}: {e:#}");
                    }
                } else if !active && !running {
                    // Бот в перерыве, не работает.
                    if needs_check || schedule_arc.park_in_hub() {
                        // Не знаем привязку или не привязан — пробуем зайти.
                        info!(
                            "schedule an{an}: {name} (idx={idx}) в перерыве, но привязка={:?} — пробую зайти (проверка)",
                            binding,
                        );
                        if let Err(e) = spawn_worker(
                            st_sched.clone(),
                            go_sched.clone(),
                            restart_sched.clone(),
                            name.clone(),
                        )
                        .await
                        {
                            error!("schedule spawn-check {name}: {e:#}");
                        }
                    }
                    // Если привязан (`true`) — не спавним, отдыхает.
                } else if !active && running {
                    // Бот в перерыве, работает.
                    // A2. Текущий казначей — не убиваем.
                    if treasurer.as_deref() == Some(name.as_str()) {
                        info!(
                            "schedule an{an}: {name} (idx={idx}) в перерыве, но это КАЗНАЧЕЙ — не убиваем"
                        );
                        continue;
                    }
                    // Казначей не уходит в сон, пока не отдал flush.
                    if flush_waiting.contains(name) {
                        info!(
                            "schedule an{an}: {name} (idx={idx}) в перерыве, но flush в полёте — НЕ убиваем"
                        );
                        continue;
                    }
                    // Разбужен досрочно для передачи денег — не убиваем.
                    if st_sched.lock().await.early_wake.contains_key(name) {
                        info!(
                            "schedule an{an}: {name} разбужен досрочно для передачи денег — не убиваю"
                        );
                        continue;
                    }
                    // Если привязка не `true` — не убиваем (у бота нет перерыва).
                    if !is_bound {
                        info!(
                            "schedule an{an}: {name} (idx={idx}) в перерыве, но привязка={:?} — не убиваем (перерыва нет)",
                            binding,
                        );
                        continue;
                    }
                    // Ворота: если пора спать (или уже спит по плану), но
                    // замены нет — откладываем сон до MAX_SLEEP_DEFER_MS.
                    if let Some((sleep_start, _)) = schedule_arc.long_break_span(idx, now) {
                        let replacement = order.iter().enumerate().any(|(j, n)| {
                            n != name
                                && ready.contains(n)
                                && !schedule_arc.is_in_long_break(j, now)
                        });
                        let overdue = now.saturating_sub(sleep_start);
                        if schedule_arc.keep_cover() && !replacement && overdue < MAX_SLEEP_DEFER_MS {
                            info!(
                                "schedule an{an}: {name} (idx={idx}) пора спать, замены нет — откладываю ({}м из 90)",
                                overdue / 60_000
                            );
                            continue;
                        }
                    }
                    if schedule_arc.park_in_hub() {
                        let first = st_sched.lock().await.parked.insert(name.clone());
                        if first {
                            info!("schedule an{an}: {name} (idx={idx}) уходит в перерыв — в хаб (онлайн)");
                            let _ = send_cmd_to(&st_sched, name, &OrchCmd::Park { on: true }).await;
                        }
                        continue;
                    }
                    info!("schedule an{an}: {name} (idx={idx}) уходит в перерыв (привязан)");
                    kill_worker(&st_sched, name).await;
                }
            }

            // Казначей спит по плану дольше лимита, замены нет → будим того,
            // у кого сон кончается раньше.
            if let Some(t) = treasurer.as_deref() {
                if let Some(ti) = order.iter().position(|n| n == t) {
                    if let Some((s, _)) = schedule_arc.long_break_span(ti, now) {
                        let has_repl = order.iter().enumerate().any(|(j, n)| {
                            n != t
                                && ready.contains(n)
                                && !schedule_arc.is_in_long_break(j, now)
                        });
                        if schedule_arc.keep_cover() && !has_repl && now.saturating_sub(s) >= MAX_SLEEP_DEFER_MS {
                            let pick = {
                                let g = st_sched.lock().await;
                                if !g.early_wake.is_empty() {
                                    None // уже будим кого-то
                                } else {
                                    order
                                        .iter()
                                        .enumerate()
                                        .filter(|(j, n)| {
                                            *j != ti
                                                && !alive.contains(*n)
                                                && g.bots.get(*n).map_or(false, |b| {
                                                    !b.banned && !b.manual_stop
                                                })
                                        })
                                        .filter_map(|(j, n)| {
                                            schedule_arc
                                                .long_break_span(j, now)
                                                .map(|(_, e)| (n.clone(), e))
                                        })
                                        .min_by_key(|(_, e)| *e)
                                }
                            };
                            if let Some((nick, end)) = pick {
                                warn!(
                                    "schedule an{an}: казначей {t} застрял, будим досрочно {nick} (до {end})"
                                );
                                st_sched.lock().await.early_wake.insert(nick.clone(), end);
                                let _ = spawn_worker(
                                    st_sched.clone(),
                                    go_sched.clone(),
                                    restart_sched.clone(),
                                    nick,
                                )
                                .await;
                            }
                        }
                    }
                }
            }

            sleep(Duration::from_secs(60)).await;
        }
    });

    // ============ казначей: тикер ============
    let st_treas = state.clone();
    let mynick_root = farm_root.clone();
    let mynick_anarchy = anarchy;
    tokio::spawn(async move {
        let mut last_peer_pay: HashMap<String, std::time::Instant> = HashMap::new();
        let mut last_dump: HashMap<String, std::time::Instant> = HashMap::new();
        // Когда впервые увидели ник в `flush_waiting`: зависший flush не должен вечно блокировать выравнивание.
        let mut flush_seen: HashMap<String, std::time::Instant> = HashMap::new();
        let mut peer_pay_log: std::collections::VecDeque<std::time::Instant> = std::collections::VecDeque::new();
        sleep(Duration::from_secs(5)).await;
        loop {
            let now = schedule::now_unix_ms();
            let (order, schedule_arc, alive, early) = {
                let mut g = st_treas.lock().await;
                g.early_wake.retain(|_, end| *end > now); // после конца сна — обычный режим
                let alive: std::collections::HashSet<String> =
                    g.workers.keys().cloned().collect();
                (
                    g.bot_order.clone(),
                    g.schedule.clone(),
                    alive,
                    g.early_wake.clone(),
                )
            };

            // «Спит» с учётом досрочного пробуждения.
            let asleep = |idx: usize, name: &str| {
                schedule_arc.is_in_long_break(idx, now) && !early.contains_key(name)
            };
            // Запас до следующего длинного сна; у разбуженного отсчёт от
            // конца текущего сна.
            let runup_of = |idx: usize, name: &str| {
                let from = early.get(name).copied().unwrap_or(now);
                schedule_arc
                    .next_treasurer_break_at(idx, from)
                    .saturating_sub(now)
            };

            let mut best: Option<(usize, String, u64)> = None;
            for (idx, name) in order.iter().enumerate() {
                if asleep(idx, name) {
                    continue;
                }
                // Казначеем может быть только активный бот: не в дневном (коротком) перерыве тоже.
                if schedule_arc.is_in_short_break(idx, now) && !early.contains_key(name) {
                    continue;
                }
                if !alive.contains(name) {
                    continue;
                }
                let runup = runup_of(idx, name);
                if runup < TREASURER_MIN_RUNUP_MS {
                    continue;
                }
                // Для сравнения «кто дольше протянет» используем абсолютное
                // время конца сна/следующего сна.
                let next_break = now + runup;
                match &best {
                    None => best = Some((idx, name.clone(), next_break)),
                    Some((_, _, b)) if next_break > *b => {
                        best = Some((idx, name.clone(), next_break));
                    }
                    _ => {}
                }
            }

            // Боты, у которых смена кончится меньше чем через 10 минут: денег не получают и не отдают
            // выравниванием (см. plan_dump).
            let leaving: std::collections::HashSet<String> = order
                .iter()
                .enumerate()
                .filter(|(idx, name)| {
                    !asleep(*idx, name.as_str())
                        && alive.contains(*name)
                        && runup_of(*idx, name.as_str()) < LEAVING_SOON_MS
                })
                .map(|(_, n)| n.clone())
                .collect();

            let new_treasurer = best.map(|(_, n, _)| n);
            let (old_treasurer, changed, need_flush_to) = {
                let mut g = st_treas.lock().await;
                let old = g.treasurer_nick.clone();
                let changed = old != new_treasurer;
                let need_flush_to = if changed {
                    if let Some(old_nick) = old.as_ref() {
                        if let Some(new_nick) = new_treasurer.as_ref() {
                            if old_nick != new_nick {
                                if g.flush_waiting.contains(old_nick) {
                                    warn!(
                                        "treasurer change: {old_nick} уже в flush_waiting — не дублирую FlushToTreasurer"
                                    );
                                    None
                                } else {
                                    g.owes_treasurer
                                        .insert(old_nick.clone(), new_nick.clone());
                                    Some((old_nick.clone(), new_nick.clone()))
                                }
                            } else {
                                None
                            }
                        } else {
                            warn!(
                                "treasurer change: {old_nick} отдал бы деньги, но новый казначей = None"
                            );
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                if changed {
                    if let Some(n) = new_treasurer.as_ref() {
                        g.owes_treasurer.remove(n); // новый казначей никому не должен
                    }
                    g.treasurer_nick = new_treasurer.clone();
                }
                (old, changed, need_flush_to)
            };

            if changed {
                info!(
                    "treasurer an{}: {:?} → {:?}",
                    anarchy, old_treasurer, new_treasurer
                );
                let role_cmd = OrchCmd::TreasurerRole {
                    username: new_treasurer.clone().unwrap_or_default(),
                };
                let _ = broadcast_cmd(&st_treas, &role_cmd).await;
            }

            if let Some((old_nick, to)) = need_flush_to {
                info!("treasurer flush: {old_nick} → {to} (перед сном)");
                let cmd = OrchCmd::FlushToTreasurer { to: to.clone() };
                if let Err(e) = send_cmd_to(&st_treas, &old_nick, &cmd).await {
                    warn!("FlushToTreasurer → {old_nick}: {e:#}");
                } else {
                    let mut g = st_treas.lock().await;
                    g.flush_waiting.insert(old_nick.clone());
                }
            }

            if let Some(treasurer) = new_treasurer.as_ref() {
                let snapshot: Vec<(String, f64)> = {
                    let g = st_treas.lock().await;
                    g.bots
                        .values()
                        .filter(|rt| !rt.banned && rt.success && rt.on_anarchy)
                        .map(|rt| {
                            (
                                rt.cfg.username.clone(),
                                rt.balance.unwrap_or(BALANCE_UNKNOWN),
                            )
                        })
                        .collect()
                };
                if !snapshot.is_empty() {
                    let cmd = OrchCmd::BalancesSnapshot { balances: snapshot };
                    if let Err(e) = send_cmd_to(&st_treas, treasurer, &cmd).await {
                        warn!("BalancesSnapshot → {treasurer}: {e:#}");
                    }
                }
            }

            // Выравнивание денег между активными ботами этой анархии.
            {
                let balances: Vec<(String, f64)> = {
                    let g = st_treas.lock().await;
                    let t_now = std::time::Instant::now();
                    flush_seen.retain(|n, _| g.flush_waiting.contains(n));
                    for n in g.flush_waiting.iter() {
                        flush_seen.entry(n.clone()).or_insert(t_now);
                    }
                    g.bots
                        .values()
                        .filter(|rt| !rt.banned && rt.success && rt.on_anarchy)
                        .filter(|rt| {
                            // flush в полёте — не трогаем, но не дольше FLUSH_BLOCK_MAX
                            match flush_seen.get(&rt.cfg.username) {
                                Some(t0) => t_now.duration_since(*t0) >= FLUSH_BLOCK_MAX,
                                None => true,
                            }
                        })
                        .filter_map(|rt| rt.balance.map(|b| (rt.cfg.username.clone(), b)))
                        .collect()
                };
                let balance_at: HashMap<String, std::time::Instant> = {
                    let g = st_treas.lock().await;
                    g.bots
                        .values()
                        .filter_map(|rt| rt.balance_at.map(|t| (rt.cfg.username.clone(), t)))
                        .collect()
                };
                let (leavers, balances): (Vec<(String, f64)>, Vec<(String, f64)>) =
                    balances.into_iter().partition(|(n, _)| leaving.contains(n));
                let equalizing = balances.iter().any(|(_, b)| *b < poor_below());
                let names: Vec<String> = balances.iter().map(|(n, _)| n.clone()).collect();
                for n in names {
                    let _ = send_cmd_to(&st_treas, &n, &OrchCmd::Equalizing { on: equalizing }).await;
                }
                let now = std::time::Instant::now();
                last_peer_pay.retain(|_, t| now.duration_since(*t) < PEER_PAY_COOLDOWN);
                last_dump.retain(|_, t| now.duration_since(*t) < DUMP_COOLDOWN);
                while peer_pay_log.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600)) {
                    peer_pay_log.pop_front();
                }
                // Уходящий в перерыв бот отдаёт всё казначею, если у того меньше 13кк.
                let treasurer_bal = new_treasurer.as_ref().and_then(|t| {
                    balances.iter().find(|(n, _)| n == t).map(|(_, b)| (t.clone(), *b))
                });
                for (from, to, amount) in plan_dump(&leavers, treasurer_bal.as_ref()) {
                    if last_dump.contains_key(&from) {
                        continue;
                    }
                    info!("перерыв скоро: {from} → казначею {to} · {amount} (всё)");
                    let cmd = OrchCmd::PayPeer { to: to.clone(), amount };
                    match send_cmd_to(&st_treas, &from, &cmd).await {
                        Ok(_) => {
                            last_dump.insert(from, now);
                        }
                        Err(e) => warn!("PayPeer → {from}: {e:#}"),
                    }
                }
                // Лишние деньги у казначея → зовём myNick забрать (заходит, ждёт перевод, выходит).
                if let (Some((t, bal)), Some(mcfg)) = (treasurer_bal.as_ref(), mynick::load(&mynick_root)) {
                    let fresh = balance_at
                        .get(t)
                        .is_some_and(|b| b.elapsed() < mynick::BALANCE_FRESH);
                    // после бана (любой анархии) — аварийный режим: ниже порог и короче пауза
                    let emergency = mynick::emergency_active(&mynick_root, mcfg.emergency_window_min);
                    let (thr, cd) = if emergency {
                        (mcfg.emergency_threshold.min(mcfg.threshold), mcfg.emergency_cooldown_min.min(mcfg.cooldown_min))
                    } else {
                        (mcfg.threshold, mcfg.cooldown_min)
                    };
                    if fresh
                        && mynick::excess_enough(*bal, thr)
                        && mynick::lock_free(&mynick_root, cd)
                    {
                        info!(
                            "mynick{}: у казначея {t} {} (лишних ≥ {}) → зову {}",
                            if emergency { " (АВАРИЙНО, был бан)" } else { "" },
                            format_money(*bal),
                            format_money(thr),
                            mcfg.username
                        );
                        if let Err(e) = mynick::spawn(&mynick_root, mynick_anarchy, &mcfg, &mynick_bin) {
                            warn!("mynick spawn: {e:#}");
                        }
                    }
                }
                let free: Vec<(String, f64)> = balances
                    .into_iter()
                    .filter(|(n, _)| match last_peer_pay.get(n) {
                        None => true,
                        // свободен, когда после перевода пришёл новый баланс
                        Some(t0) => balance_at.get(n).is_some_and(|b| b.duration_since(*t0) >= PEER_PAY_SETTLE),
                    })
                    .collect();
                let budget = MAX_PEER_PAYS_PER_HOUR.saturating_sub(peer_pay_log.len());
                for (from, to, amount) in plan_rebalance(&free).into_iter().take(budget) {
                    info!("выравнивание: {from} → {to} · {amount}");
                    pretty_log::line("orch", LineKind::Info, &format!("выравнивание: {from} → {to} · {}", format_money(amount as f64)));
                    let cmd = OrchCmd::PayPeer { to: to.clone(), amount };
                    match send_cmd_to(&st_treas, &from, &cmd).await {
                        Ok(_) => {
                            last_peer_pay.insert(from, now);
                            last_peer_pay.insert(to, now);
                            peer_pay_log.push_back(now);
                        }
                        Err(e) => warn!("PayPeer → {from}: {e:#}"),
                    }
                }
            }

            sleep(TREASURER_TICK).await;
        }
    });

    {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                r = tokio::signal::ctrl_c() => { r?; }
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
    }
    info!("signal → shutdown");
    {
        let mut g = state.lock().await;
        g.shutting_down = true;
        for (name, _wh) in g.workers.drain() {
            info!("stop {name}");
        }
    }
    Ok(())
}

// ============ load: bindings.json ============

fn load_bindings(path: &Path) -> HashMap<String, bool> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, bool>>(&raw) {
        Ok(m) => m,
        Err(e) => {
            warn!("bindings.json parse: {e} — пустой");
            HashMap::new()
        }
    }
}

/// `bindings.json` общий для всех оркестраторов. Раньше каждый писал свою копию целиком
/// (последний затирал чужие записи) → после рестарта у бота привязка `None`, и он не уходил в перерыв.
/// Теперь: блокировка `bindings.json.lock`, перечитать файл, поменять ОДНУ запись, атомарно записать.
fn save_binding(path: &Path, nick: &str, value: bool) {
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path);
    let lock = match lock {
        Ok(f) => f,
        Err(e) => {
            warn!("bindings.json lock open: {e}");
            return;
        }
    };
    if let Err(e) = lock.lock() {
        warn!("bindings.json lock: {e}");
        return;
    }
    let mut map = load_bindings(path);
    map.insert(nick.to_string(), value);
    if let Ok(raw) = serde_json::to_string_pretty(&map) {
        let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
        if std::fs::write(&tmp, raw).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, path) {
                warn!("bindings.json rename: {e}");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    let _ = lock.unlock();
}

/// Воркеры прошлого запуска этой анархии, оставшиеся без оркестратора (ppid=1): они играют без
/// расписания и занимают ник («Игрок с таким же ником уже онлайн»). Метка — env `FARM_ANARCHY`.
#[cfg(target_os = "linux")]
fn kill_orphan_workers(anarchy: u16) {
    let want = format!("FARM_ANARCHY={anarchy}");
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // pid (comm) state ppid ...
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|x| x.parse::<u32>().ok());
        if ppid != Some(1) {
            continue;
        }
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        if env.split(|b| *b == 0).any(|kv| kv == want.as_bytes()) {
            warn!("сирота-воркер pid={pid} ({want}) — убиваю");
            let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn kill_orphan_workers(_anarchy: u16) {}

// ============ load: farm ============

fn load_bots(
    bots_path: &Path,
    ip_path: &Path,
) -> Result<(HashMap<String, BotRuntime>, Vec<String>)> {
    let raw = std::fs::read_to_string(bots_path)?;
    let list: Vec<BotJson> = serde_json::from_str(&raw)?;
    // ip.json теперь необязателен: прокси берётся из поля `proxy` бота, ip.json — запасной вариант
    let ip_map: HashMap<String, String> = match std::fs::read_to_string(ip_path) {
        Ok(raw) => serde_json::from_str(&raw)?,
        Err(_) => HashMap::new(),
    };
    let mem_path = bans_memory_path(bots_path);
    let remembered = match list.first().map(|b| b.anarchy) {
        Some(my_anarchy) => prune_remembered_bans(
            &mem_path,
            load_remembered_bans(&mem_path),
            list.iter().map(|b| b.username.to_lowercase()).collect(),
            my_anarchy as u32,
        ),
        None => load_remembered_bans(&mem_path),
    };
    let mut out = HashMap::new();
    let mut order = Vec::new();
    for b in list {
        order.push(b.username.clone());
        let proxy = if !b.proxy.trim().is_empty() {
            b.proxy.trim().to_string()
        } else {
            ip_map.get(&b.ip).cloned().with_context(|| {
                format!(
                    "farm {}: нет `proxy` в конфиге бота и нет ip-слота {:?} в ip.json",
                    b.username, b.ip
                )
            })?
        };
        let key = b.username.to_lowercase();
        let remembered_ban = remembered.get(&key).cloned();
        let banned = remembered_ban.is_some();
        out.insert(
            b.username.clone(),
            BotRuntime {
                cfg: b,
                proxy,
                success: false,
                on_anarchy: false,
                banned,
                manual_stop: banned,
                unstable_until: None,
                pricing_edges: std::collections::VecDeque::new(),
                last_pricing_ok: false,
                items: json!({}),
                inventory: json!([]),
                balance: None,
                balance_at: None,
                banned_at: remembered_ban.as_ref().and_then(|v| {
                    v.get("banned_at")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                }),
                banned_reason: remembered_ban.as_ref().and_then(|v| {
                    v.get("reason")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                }),
                auth_fault: None,
                auth_fault_at: None,
                auth_fault_reason: None,
                spawning: false,
                rent_until_ms: 0,
            },
        );
    }
    info!(
        "bots.json: {} farm-ботов (бан в памяти: {})",
        out.len(),
        out.values().filter(|b| b.banned).count()
    );
    Ok((out, order))
}

// ============ load: my_nick ============

/// Ник, на который казначеи сливают излишек, — `username` из `mynick.json` (единственный источник).
/// `clan-owners.json` больше не читается: он держал старый ник, и казначеи платили не тому.
fn load_my_nick(farm_root: &Path) -> Result<Option<String>> {
    let my_nick = mynick::load(farm_root)
        .map(|c| c.username.trim().to_string())
        .filter(|n| !n.is_empty());
    if my_nick.is_none() {
        warn!("mynick.json выключен или без username — myNick = None, слив на ник отключён");
    }
    Ok(my_nick)
}

// ============ ban-memory ============

fn bans_memory_path(bots_path: &Path) -> PathBuf {
    bots_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("banned-memory.json")
}

fn load_remembered_bans(path: &Path) -> HashMap<String, Value> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, Value>>(&raw) else {
        return HashMap::new();
    };
    map
}

/// Анархия, к которой относится запись о бане: поле `anarchy`, а у старых записей — из текста
/// причины («…к серверу Anarchy504…»). `None` — не удалось понять, такую запись не трогаем.
fn ban_anarchy(entry: &Value) -> Option<u32> {
    if let Some(a) = entry.get("anarchy").and_then(|v| v.as_u64()) {
        return Some(a as u32);
    }
    let reason = entry.get("reason")?.as_str()?;
    let rest = &reason[reason.find("Anarchy")? + "Anarchy".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Ник сменили в конфиге — убираем бан только если запись относится к ЭТОЙ анархии и ника
/// нет в её конфиге. `banned-memory.json` общий для всех анархий, чужие записи не трогаем;
/// записи ботов, которые в конфиге есть, остаются (они забанены). В файл пишем, только если
/// что-то удалили.
fn prune_remembered_bans(
    path: &Path,
    mut map: HashMap<String, Value>,
    current: std::collections::HashSet<String>,
    my_anarchy: u32,
) -> HashMap<String, Value> {
    let before = map.len();
    map.retain(|nick, entry| {
        current.contains(nick) || ban_anarchy(entry) != Some(my_anarchy)
    });
    if map.len() != before {
        info!(
            "banned-memory: убрано {} ников анархии {my_anarchy}, которых нет в конфиге",
            before - map.len()
        );
        if let Ok(raw) = serde_json::to_string_pretty(&map) {
            let _ = std::fs::write(path, raw);
        }
    }
    map
}

fn remember_ban(
    path: &Path,
    username: &str,
    anarchy: u32,
    reason: Option<&str>,
    banned_at: Option<&str>,
) {
    let mut map = load_remembered_bans(path);
    map.insert(
        username.to_lowercase(),
        json!({
            "username": username,
            "anarchy": anarchy,
            "reason": reason.unwrap_or(""),
            "banned_at": banned_at.unwrap_or(""),
        }),
    );
    if let Ok(raw) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, raw);
    }
}

// ============ start / spawn ============

async fn start_all(state: Arc<Mutex<OrchState>>, go_tx: GoTx, restart_tx: RestartTx) {
    let (order, schedule_arc, bindings) = {
        let g = state.lock().await;
        (g.bot_order.clone(), g.schedule.clone(), g.bindings.clone())
    };

    let now = schedule::now_unix_ms();
    for (idx, name) in order.iter().enumerate() {
        if idx > 0 {
            sleep(Duration::from_secs(8)).await;
        }
        let is_bound = bindings.get(name).copied() == Some(true);
        let active = schedule_arc.is_active(idx, now);
        // Спавним, если активен ИЛИ привязка не `true` (надо проверить).
        if !active && is_bound && !schedule_arc.park_in_hub() {
            info!("{name} (idx={idx}): в перерыве и привязан, spawn пропущен");
            continue;
        }
        if !active {
            info!("{name} (idx={idx}): в перерыве, но привязка не true — spawn (проверка)");
        }
        if let Err(e) =
            spawn_worker(state.clone(), go_tx.clone(), restart_tx.clone(), name.clone()).await
        {
            error!("spawn {name}: {e:#}");
        }
    }
}

async fn kill_worker(state: &Arc<Mutex<OrchState>>, username: &str) {
    let mut g = state.lock().await;
    g.parked.remove(username);
    if let Some(mut wh) = g.workers.remove(username) {
        info!("{username}: kill_worker (перерыв)");
        let _ = wh.child.start_kill();
    }
}

async fn spawn_worker(
    state: Arc<Mutex<OrchState>>,
    go_tx: GoTx,
    restart_tx: RestartTx,
    username: String,
) -> Result<()> {
    let (boot, bot_bin) = {
        let mut g = state.lock().await;
        if g.shutting_down {
            return Ok(());
        }
        let (banned, manual_stop, spawning) = {
            let rt = g.bots.get(&username).context("unknown bot")?;
            (rt.banned, rt.manual_stop, rt.spawning)
        };
        if banned || manual_stop {
            warn!("skip {username} (banned/manual_stop)");
            return Ok(());
        }
        if spawning {
            warn!("{username}: spawn уже идёт — skip");
            return Ok(());
        }
        if g.workers.contains_key(&username) {
            warn!("{username}: уже жив — spawn skip");
            return Ok(());
        }
        let boot = {
            let my_nick = g.my_nick.clone();
            let treasurer_nick = g.treasurer_nick.clone();
            let rt = g.bots.get_mut(&username).context("unknown bot")?;
            rt.success = false;
            rt.spawning = true;
            BotBoot {
                username: rt.cfg.username.clone(),
                password: rt.cfg.password.clone(),
                anarchy: rt.cfg.anarchy,
                item: rt.cfg.item.clone(),
                go_type: rt.cfg.go_type.clone(),
                proxy: rt.proxy.clone(),
                server: "mc.funtime.su".into(),
                my_nick,
                owner_nick: None,
                owner_reason: None,
                treasurer_nick,
                warmup: false,
            }
        };
        (boot, g.bot_bin.clone())
    };

    let result =
        spawn_worker_inner(state.clone(), go_tx, restart_tx, boot, bot_bin).await;

    {
        let mut g = state.lock().await;
        if let Some(rt) = g.bots.get_mut(&username) {
            rt.spawning = false;
        }
    }

    result
}

/// `pricing-experiment.json` рядом с schedule.json: {"508": {"extra_margin_pct": 7}} — доп. маржа закупки (% от цены
/// продажи) для воркеров этой анархии. Нет файла/записи → 0 (поведение как раньше).
fn experiment_extra_margin(anarchy: u16) -> f64 {
    let Ok(raw) = std::fs::read_to_string("pricing-experiment.json") else {
        return 0.0;
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get(anarchy.to_string())?.get("extra_margin_pct")?.as_f64())
        .unwrap_or(0.0)
}

async fn spawn_worker_inner(
    state: Arc<Mutex<OrchState>>,
    go_tx: GoTx,
    restart_tx: RestartTx,
    boot: BotBoot,
    bot_bin: PathBuf,
) -> Result<()> {
    if !bot_bin.exists() {
        bail!("нет бинаря {} — сначала: cargo build -p bot", bot_bin.display());
    }

    pretty_log::line(&boot.username, LineKind::Info, "spawn");
    let extra_margin = experiment_extra_margin(boot.anarchy);
    let mut cmd_bot = Command::new(&bot_bin);
    cmd_bot.env("FARM_ANARCHY", boot.anarchy.to_string());
    if extra_margin > 0.0 {
        cmd_bot.env("BUY_EXTRA_MARGIN_PCT", extra_margin.to_string());
    }
    let mut child = cmd_bot
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bot_bin.display()))?;

    let child_pid = child.id();
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let boot_line = serde_json::to_string(&boot)? + "\n";
    stdin.write_all(boot_line.as_bytes()).await?;

    {
        let st = {
            let mut g = state.lock().await;
            g.listings.export_for(&boot.username)
        };
        let cmd = OrchCmd::ListingState { data: st };
        let line = serde_json::to_string(&cmd)? + "\n";
        stdin.write_all(line.as_bytes()).await?;
    }
    stdin.flush().await?;

    let uname = boot.username.clone();
    {
        let mut g = state.lock().await;
        g.workers.insert(uname.clone(), WorkerHandle { child, stdin });
    }

    // Режим «перерыв в хабе»: привязанный бот, который стартует в перерыве, на анку не заходит.
    {
        let park = {
            let mut g = state.lock().await;
            let idx = g.bot_order.iter().position(|n| n == &uname);
            let bound = g.bindings.get(&uname).copied() == Some(true);
            let is_treas = g.treasurer_nick.as_deref() == Some(uname.as_str());
            let early = g.early_wake.contains_key(&uname);
            let want = g.schedule.park_in_hub()
                && bound
                && !is_treas
                && !early
                && idx.map_or(false, |i| !g.schedule.is_active(i, schedule::now_unix_ms()));
            if want {
                g.parked.insert(uname.clone());
            }
            want
        };
        if park {
            info!("spawn {uname}: перерыв → сразу в хаб, на анку не захожу");
            let _ = send_cmd_to(&state, &uname, &OrchCmd::Park { on: true }).await;
        }
    }

    // B3. owes_treasurer — досылаем FlushToTreasurer.
    {
        let owes_to = {
            let mut g = state.lock().await;
            match g.owes_treasurer.remove(&uname) {
                None => None,
                Some(old_to) => match g.treasurer_nick.clone() {
                    Some(t) if t == uname => None, // сам казначей, долг аннулирован
                    Some(t) => Some(t),            // шлём актуальному казначею
                    None => Some(old_to),
                },
            }
        };
        if let Some(to) = owes_to {
            info!("spawn {uname}: owes_treasurer → {to}, досылаю FlushToTreasurer");
            let cmd = OrchCmd::FlushToTreasurer { to: to.clone() };
            match send_cmd_to(&state, &uname, &cmd).await {
                Ok(()) => {
                    let mut g = state.lock().await;
                    g.flush_waiting.insert(uname.clone());
                }
                Err(e) => {
                    warn!("spawn {uname}: не удалось дослать FlushToTreasurer: {e:#}");
                    let mut g = state.lock().await;
                    g.owes_treasurer.insert(uname.clone(), to);
                }
            }
        }
    }

     // ============================================================
    // `bindings = true` ставит ТОЛЬКО `funauth_verified`
    // (см. `handle_worker_line`). 5с-таймер от spawn не нужен —
    // бот не успевает зайти за 5с (логин + `/an503` + scoreboard),
    // поэтому таймер ставил `false` даже при наличии привязки.
    //
    // `bindings = false` ставит:
    //   - `funauth_bind` (`/vk`) — привязки нет;
    //   - `funauth_2fa` — привязки нет;
    //   - `auth_fault` — привязки/пароля нет.
    //
    // Если бот вообще не зашёл (нет `funauth_verified`), `bindings`
    // остаётся `None` → `schedule tick` будет спавнить на проверку.
    // ============================================================

    let st = state.clone();
    let go = go_tx.clone();
    let restart = restart_tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Err(e) = handle_worker_line(&st, &go, &uname, &line).await {
                warn!("{uname} ipc: {e:#}");
            }
        }
        drop(lines);
        let (code, stale_exit) = {
            let mut g = st.lock().await;
            let live_pid = g.workers.get(&uname).and_then(|wh| wh.child.id());
            if live_pid.is_some() && live_pid != child_pid {
                warn!("{uname}: умер старый pid={child_pid:?}, живой pid={live_pid:?} — не трогаю");
                (None, true)
            } else if let Some(mut wh) = g.workers.remove(&uname) {
                g.parked.remove(&uname);
                (
                    wh.child.try_wait().ok().flatten().and_then(|s| s.code()),
                    false,
                )
            } else {
                (None, false)
            }
        };
        if stale_exit {
            return;
        }
        pretty_log::line(&uname, LineKind::Warn, &format!("exit code={code:?}"));

        let should_restart = {
            let mut g = st.lock().await;
            let shutting = g.shutting_down;

            let was_waiting_flush = g.flush_waiting.remove(&uname);
            if was_waiting_flush {
                let existing_owes = g.owes_treasurer.get(&uname).cloned();
                let target = existing_owes
                    .or_else(|| g.treasurer_nick.clone().filter(|t| t != &uname));
                if let Some(to) = target {
                    g.owes_treasurer.insert(uname.clone(), to.clone());
                    warn!(
                        "{uname}: умер, не дослав treasurer_flushed → owes {to}, дошлём при респавне"
                    );
                } else {
                    warn!(
                        "{uname}: умер, не дослав treasurer_flushed, treasurer_nick — сам {uname} или None — flush не требуется/потерян"
                    );
                }
            }

            if let Some(rt) = g.bots.get_mut(&uname) {
                rt.success = false;
                rt.on_anarchy = false;
                note_pricing_edge(rt, false);
                rt.items = json!({});
                rt.inventory = json!([]);
                if let Some(kind) = auth_fault_kind_from_exit(code) {
                    let now = chrono_like_now();
                    rt.auth_fault = Some(kind.to_string());
                    rt.auth_fault_at = Some(now);
                    if rt.auth_fault_reason.is_none() {
                        rt.auth_fault_reason = Some(format!("exit {code:?}"));
                    }
                    if kind == "bad_password" {
                        rt.manual_stop = true;
                    }
                }
                let block = rt.manual_stop
                    || rt.banned
                    || rt.auth_fault.as_deref() == Some("bad_password");

                let in_break = {
                    let idx = g.bot_order.iter().position(|n| n == &uname);
                    match idx {
                        Some(i) => {
                            let now = schedule::now_unix_ms();
                            !g.schedule.is_active(i, now)
                        }
                        None => false,
                    }
                };

                !shutting && !block && !in_break
            } else {
                false
            }
        };

        let _ = push_presence(&st, &go).await;
        if should_restart {
            pretty_log::line(&uname, LineKind::Info, "очередь рестарта");
            let _ = restart.send(uname);
        }
    });

    Ok(())
}

// ============ handle worker line ============

async fn handle_worker_line(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    username: &str,
    line: &str,
) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let is_treasurer = {
        let g = state.lock().await;
        g.treasurer_nick.as_deref() == Some(username)
    };

    if line.starts_with('"') {
        pretty_log::line_ex(
            username,
            LineKind::Info,
            line.trim_matches('"'),
            is_treasurer,
        );
        return Ok(());
    }
    let msg: WorkerMsg = serde_json::from_str(line)?;

    match msg.name.as_str() {
        "success" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.success = true;
                rt.on_anarchy = true;
                rt.auth_fault = None;
                rt.auth_fault_at = None;
                rt.auth_fault_reason = None;
                note_pricing_edge(rt, true);
            }
            pretty_log::line_ex(
                username,
                LineKind::Ok,
                "на анархии → success",
                is_treasurer,
            );
            drop(g);
            let _ = push_presence(state, go_tx).await;
            let _ = send_fleet(state, go_tx).await;
            let _ = go_tx.send(json!({ "action": "info" }));
        }
        "hub" => {
            let why = msg.reason.as_deref().unwrap_or("?");
            pretty_log::line_ex(
                username,
                LineKind::Warn,
                &format!("хаб / не на анке: {why}"),
                is_treasurer,
            );
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.on_anarchy = false;
                note_pricing_edge(rt, false);
            }
            drop(g);
            let _ = push_presence(state, go_tx).await;
        }
        "items" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                if let Some(items) = msg.items {
                    rt.items = items;
                }
            }
            drop(g);
            let _ = push_presence(state, go_tx).await;
        }
        "inventory" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                if let Some(data) = msg.data {
                    rt.inventory = data;
                }
            }
        }
        "kicked" | "banned" => {
            let kind = if msg.name == "banned" { "бан" } else { "кик" };
            pretty_log::line_ex(
                username,
                LineKind::Err,
                &format!("{kind}: {}", msg.reason.as_deref().unwrap_or("?")),
                is_treasurer,
            );
            if msg.name == "banned" {
                let (memory_path, banned_at, reason, ban_anarchy) = {
                    let mut g = state.lock().await;
                    let path = bans_memory_path(&g.bots_path);
                    if let Some(rt) = g.bots.get_mut(username) {
                        rt.banned = true;
                        rt.manual_stop = true;
                        rt.banned_at = Some(chrono_like_now());
                        rt.banned_reason = msg.reason.clone();
                        (path, rt.banned_at.clone(), rt.banned_reason.clone(), rt.cfg.anarchy as u32)
                    } else {
                        (path, None, None, 0)
                    }
                };
                if let Some(root) = memory_path.parent().and_then(|b| b.parent()) {
                    mynick::record_ban(root);
                }
                remember_ban(
                    &memory_path,
                    username,
                    ban_anarchy,
                    reason.as_deref().or(msg.reason.as_deref()),
                    banned_at.as_deref(),
                );
            }
            let _ = push_presence(state, go_tx).await;
            let _ = send_fleet(state, go_tx).await;
        }
        "buy" | "sell" | "try-sell" => {
            forward_trade(go_tx, &msg)?;
        }
        "set_min_price" => {
            let price = msg.price.map(|p| p.round() as i64);
            send_go(
                go_tx,
                json!({ "action": "set_min_price", "type": msg.id, "price": price }),
            )?;
        }
        "buying" => {
            handle_buying_claim(state, go_tx, username, &msg).await?;
        }
        "buying_release" => {
            handle_buying_release(state, go_tx, username, &msg).await?;
        }
        "listing" => {
            let data = msg.data.clone().unwrap_or(json!({}));
            let result = {
                let mut g = state.lock().await;
                g.listings.handle(username, &data)
            };
            let cmd = OrchCmd::ListingRes {
                req_id: data.get("reqId").and_then(|v| v.as_u64()).unwrap_or(0),
                result,
            };
            send_cmd_to(state, username, &cmd).await?;
        }
        "captcha" => {
            pretty_log::line_ex(username, LineKind::Warn, "captcha", is_treasurer);
        }
        // Живость воркера (health.rs в боте): явная смерть, ошибки подключения, нет интернета.
        "worker_dead" => {
            pretty_log::line_ex(
                username,
                LineKind::Err,
                &format!("воркер мёртв: {} → перезапуск", msg.reason.as_deref().unwrap_or("?")),
                is_treasurer,
            );
        }
        // Бот заспавнился через прокси: прокси рабочий. Снимаем пометку proxy_error (bad_password не трогаем).
        "proxy_ok" => {
            let cleared = {
                let mut g = state.lock().await;
                match g.bots.get_mut(username) {
                    Some(rt) if rt.auth_fault.as_deref() == Some("proxy_error") => {
                        rt.auth_fault = None;
                        rt.auth_fault_at = None;
                        rt.auth_fault_reason = None;
                        true
                    }
                    _ => false,
                }
            };
            if cleared {
                pretty_log::line_ex(username, LineKind::Ok, "прокси снова рабочий", is_treasurer);
                let _ = push_presence(state, go_tx).await;
            }
        }
        "auth_fault" => {
            let kind = msg
                .data
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("bad_password")
                .to_string();
            {
                let mut g = state.lock().await;
                if let Some(rt) = g.bots.get_mut(username) {
                    rt.auth_fault = Some(kind.clone());
                    rt.auth_fault_at = Some(chrono_like_now());
                    rt.auth_fault_reason = msg.reason.clone();
                    if kind == "bad_password" {
                        rt.manual_stop = true;
                    }
                }
                // auth_fault — привязки нет. Пишем в bindings.json.
                // Но не для proxy_error: сбой прокси ничего не говорит о привязке аккаунта.
                if kind != "proxy_error" {
                    g.bindings.insert(username.to_string(), false);
                    save_binding(&g.bindings_path, username, false);
                }
            }
            pretty_log::line_ex(
                username,
                LineKind::Err,
                &format!("auth_fault {kind}: {}", msg.reason.as_deref().unwrap_or("?")),
                is_treasurer,
            );
            let _ = push_presence(state, go_tx).await;
        }
        "funauth_bind" => {
            pretty_log::line_ex(
                username,
                LineKind::Warn,
                "funauth bind → Go + рестарт",
                is_treasurer,
            );
            let (anarchy, password) = {
                let g = state.lock().await;
                g.bots
                    .get(username)
                    .map(|rt| (rt.cfg.anarchy, rt.cfg.password.clone()))
                    .unwrap_or((0, String::new()))
            };
            if password.is_empty() {
                warn!("funauth_bind {username}: нет пароля");
            } else {
                send_go(
                    go_tx,
                    json!({
                        "action": "funauth_bind",
                        "nick": username,
                        "password": password,
                        "anarchy": anarchy,
                    }),
                )?;
            }
            let mut g = state.lock().await;
            // funauth_bind — привязки нет. Пишем в bindings.json.
            g.bindings.insert(username.to_string(), false);
            save_binding(&g.bindings_path, username, false);
            if let Some(wh) = g.workers.get_mut(username) {
                let _ = wh.child.start_kill();
            }
        }
        "funauth_2fa" => {
            pretty_log::line_ex(
                username,
                LineKind::Warn,
                "funauth 2fa → рестарт воркера",
                is_treasurer,
            );
            let anarchy = {
                let g = state.lock().await;
                g.bots.get(username).map(|rt| rt.cfg.anarchy)
            };
            send_go(
                go_tx,
                json!({
                    "action": "funauth_2fa",
                    "nick": username,
                    "anarchy": anarchy,
                }),
            )?;
            let mut g = state.lock().await;
            // funauth_2fa — привязки нет. Пишем в bindings.json.
            g.bindings.insert(username.to_string(), false);
            save_binding(&g.bindings_path, username, false);
            if let Some(wh) = g.workers.get_mut(username) {
                let _ = wh.child.start_kill();
            }
        }
        "funauth_verified" => {
            let anarchy = {
                let g = state.lock().await;
                g.bots.get(username).map(|rt| rt.cfg.anarchy)
            };
            send_go(
                go_tx,
                json!({
                    "action": "funauth_verified",
                    "nick": username,
                    "anarchy": anarchy,
                }),
            )?;
            // **НОВОЕ:** воркер подтвердил, что 5с на анке прошло без
            // `funauth_bind` / `funauth_2fa` → привязка есть.
            // Ставим `bindings = true` и, если сейчас перерыв — сразу
            // вырубаем (не ждём следующего `schedule tick`).
            let (should_kill, bindings_path) = {
                let mut g = state.lock().await;
                g.bindings.insert(username.to_string(), true);
                let bp = g.bindings_path.clone();
                save_binding(&bp, username, true);

                let idx = g.bot_order.iter().position(|n| n == username);
                let in_break = match idx {
                    Some(i) => {
                        let now = schedule::now_unix_ms();
                        !g.schedule.is_active(i, now)
                    }
                    None => false,
                };
                let is_treasurer_now = g.treasurer_nick.as_deref() == Some(username);
                let is_flush_waiting = g.flush_waiting.contains(username);
                // Казначея и flush не убиваем — деньги важнее.
                (in_break && !is_treasurer_now && !is_flush_waiting, bp)
            };
            pretty_log::line_ex(
                username,
                LineKind::Ok,
                "bindings.json = true (funauth_verified)",
                is_treasurer,
            );
            if should_kill {
                info!(
                    "{username}: funauth_verified + сейчас перерыв → вырубаю сразу"
                );
                let park = state.lock().await.schedule.park_in_hub();
                if park {
                    // Не отключаем: уходим в хаб и стоим.
                    state.lock().await.parked.insert(username.to_string());
                    let _ = send_cmd_to(state, username, &OrchCmd::Park { on: true }).await;
                } else {
                    kill_worker(state, username).await;
                }
            }
        }
        "rent" => {
            // бот сообщает, до какого момента арендованы слоты; Go узнаёт ёмкость из presence
            let until_ms = msg
                .data
                .as_ref()
                .and_then(|d| d.get("until_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            {
                let mut g = state.lock().await;
                if let Some(rt) = g.bots.get_mut(username) {
                    rt.rent_until_ms = until_ms;
                }
            }
            let slots = slots_for(until_ms, schedule::now_unix_ms());
            pretty_log::line_ex(
                username,
                LineKind::Info,
                &format!("аренда слотов: до {until_ms} (unix-мс), слотов сейчас {slots}"),
                is_treasurer,
            );
            let _ = push_presence(state, go_tx).await;
        }
        "balance" => {
            if let Some(bal) = msg.balance {
                pretty_log::line_ex(
                    username,
                    LineKind::Ok,
                    &format!("баланс {}", format_money(bal)),
                    is_treasurer,
                );
                {
                    let mut g = state.lock().await;
                    if let Some(rt) = g.bots.get_mut(username) {
                        rt.balance = Some(bal);
                        rt.balance_at = Some(std::time::Instant::now());
                    }
                }
                push_capital_plan_to_bots(state).await?;
            }
        }
        "ah_lots" => {
            if let Some(lots) = &msg.lots {
                let lots = coerce_ah_lots_prices(lots);
                let n = lots.as_array().map(|a| a.len()).unwrap_or(0);
                if n > 0 {
                    tracing::info!("ah_lots → Go · {n} лотов");
                }
                send_go(go_tx, json!({ "action": "ah_lots", "lots": lots }))?;
            }
        }
        "staff_check" => {
            let anarchy = {
                let from_data = msg
                    .data
                    .as_ref()
                    .and_then(|d| d.get("anarchy"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16);
                if from_data.is_some() {
                    from_data
                } else {
                    let g = state.lock().await;
                    g.bots.get(username).map(|rt| rt.cfg.anarchy)
                }
            };
            pretty_log::line_ex(
                username,
                LineKind::Warn,
                "проверка читов → Go",
                is_treasurer,
            );
            send_go(
                go_tx,
                json!({
                    "action": "staff_check",
                    "username": username,
                    "anarchy": anarchy,
                    "reason": msg.reason.as_deref().unwrap_or(""),
                }),
            )?;
        }
        "my_nick_seen" => {
            let nick = msg
                .data
                .as_ref()
                .and_then(|d| d.get("nick"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            pretty_log::line_ex(
                username,
                LineKind::Info,
                &format!("my_nick_seen: {nick} в табе"),
                is_treasurer,
            );
        }
        "treasurer_flushed" => {
            let ok = msg
                .data
                .as_ref()
                .and_then(|d| d.get("ok"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let to = msg
                .data
                .as_ref()
                .and_then(|d| d.get("to"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let amount = msg
                .data
                .as_ref()
                .and_then(|d| d.get("amount"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            pretty_log::line_ex(
                username,
                if ok { LineKind::Ok } else { LineKind::Err },
                &format!(
                    "treasurer_flushed → {to} ({}), ok={ok}",
                    format_money(amount as f64)
                ),
                is_treasurer,
            );
            let mut g = state.lock().await;
            g.flush_waiting.remove(username);
            if ok {
                g.owes_treasurer.remove(username);
            } else {
                if !g.owes_treasurer.contains_key(username) {
                    if let Some(t) = g.treasurer_nick.clone().filter(|t| t != username) {
                        warn!(
                            "treasurer_flushed ok=false от {username}: owes не было — ставлю на {t}, дошлём при респавне"
                        );
                        g.owes_treasurer.insert(username.to_string(), t);
                    } else {
                        warn!(
                            "treasurer_flushed ok=false от {username}: owes не было, treasurer_nick — сам {username} или None — flush не требуется/потерян"
                        );
                    }
                } else {
                    warn!(
                        "treasurer_flushed ok=false от {username}: owes сохранён, дошлём при респавне"
                    );
                }
            }
        }
        "paid" => {
            let req_id = msg
                .data
                .as_ref()
                .and_then(|d| d.get("reqId"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let ok = msg
                .data
                .as_ref()
                .and_then(|d| d.get("ok"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let pending = {
                let mut g = state.lock().await;
                g.pending_pays.remove(&req_id)
            };
            let Some(pending) = pending else {
                warn!("paid: req_id={req_id} не найден в pending_pays");
                return Ok(());
            };
            pretty_log::line_ex(
                username,
                if ok { LineKind::Ok } else { LineKind::Err },
                &format!(
                    "paid req={req_id} → {} ({}) ok={ok}",
                    pending.to_bot,
                    format_money(pending.amount as f64),
                ),
                is_treasurer,
            );
        }
        other => {
            pretty_log::line_ex(
                username,
                LineKind::Info,
                &format!("msg={other}"),
                is_treasurer,
            );
        }
    }
    Ok(())
}

// ============ helpers ============

fn forward_trade(go_tx: &GoTx, msg: &WorkerMsg) -> Result<()> {
    let price = msg.price.map(|p| p.round() as i64);
    let mut payload = json!({
        "action": msg.name,
        "type": msg.id,
        "price": price,
    });
    if let Some(data) = &msg.data {
        if let Some(obj) = payload.as_object_mut() {
            if let Some(ench) = data.get("enchants") {
                obj.insert("enchants".into(), ench.clone());
            }
            if let Some(dur) = data.get("durability") {
                obj.insert("durability".into(), dur.clone());
            }
        }
    }
    send_go(go_tx, payload)
}

async fn handle_buying_claim(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    username: &str,
    msg: &WorkerMsg,
) -> Result<()> {
    let uuid = msg
        .data
        .as_ref()
        .and_then(|d| d.get("uuid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(uuid) = uuid else {
        return Ok(());
    };
    // Решает Go: первый заявивший владеет лотом (lot_claims.go), ответ придёт как claim_result.
    // Пока ответа нет (или Go недоступен), действует та же логика локально: первый владелец
    // остаётся, чужая заявка его не вытесняет.
    let updated = {
        let mut g = state.lock().await;
        let mut list = g.items_buying.as_array().cloned().unwrap_or_default();
        let taken = list.iter().any(|raw| {
            matches!(normalize_buying_entry(raw), Some((u, Some(owner))) if u == uuid && owner != username)
        });
        if !taken {
            list.retain(|raw| {
                !matches!(normalize_buying_entry(raw), Some((u, _)) if u == uuid)
            });
            list.push(json!({ "uuid": uuid, "username": username }));
        }
        g.items_buying = Value::Array(list);
        g.items_buying.clone()
    };
    let cmd = OrchCmd::ItemsBuying { data: updated };
    broadcast_cmd(state, &cmd).await?;
    // Сколько у бота уже накоплено (лоты на АХ + инвентарь): при нескольких заявках на один лот Go отдаёт его
    // тому, у кого накоплено меньше.
    let held = msg.data.as_ref().and_then(|d| d.get("held")).cloned().unwrap_or(json!(0));
    // через сколько мс бот кликнет по лоту
    let click_in_ms = msg.data.as_ref().and_then(|d| d.get("click_in_ms")).cloned().unwrap_or(Value::Null);
    // АХ бота уже полон: он уступает ботам, которым есть куда выставлять
    let ah_full = msg.data.as_ref().and_then(|d| d.get("ah_full")).and_then(|v| v.as_bool()).unwrap_or(false);
    send_go(
        go_tx,
        json!({ "action": "claim", "json_data": uuid, "username": username, "held": held, "ah_full": ah_full, "click_in_ms": click_in_ms }),
    )?;
    Ok(())
}

/// Бот отказался от лота (изменился, стал невыгоден): снимаем заявку, чтобы лот мог взять другой.
async fn handle_buying_release(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    username: &str,
    msg: &WorkerMsg,
) -> Result<()> {
    let Some(uuid) = msg
        .data
        .as_ref()
        .and_then(|d| d.get("uuid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Ok(());
    };
    let updated = {
        let mut g = state.lock().await;
        let mut list = g.items_buying.as_array().cloned().unwrap_or_default();
        let before = list.len();
        list.retain(|raw| {
            !matches!(normalize_buying_entry(raw), Some((u, Some(owner))) if u == uuid && owner == username)
        });
        if list.len() == before {
            None
        } else {
            g.items_buying = Value::Array(list);
            Some(g.items_buying.clone())
        }
    };
    if let Some(data) = updated {
        broadcast_cmd(state, &OrchCmd::ItemsBuying { data }).await?;
    }
    send_go(
        go_tx,
        json!({ "action": "unclaim", "json_data": uuid, "username": username }),
    )?;
    Ok(())
}

fn coerce_ah_lots_prices(lots: &Value) -> Value {
    let Some(arr) = lots.as_array() else {
        return lots.clone();
    };
    let out: Vec<Value> = arr
        .iter()
        .map(|lot| {
            let mut o = match lot.as_object() {
                Some(m) => m.clone(),
                None => return lot.clone(),
            };
            if let Some(p) = o.get("price") {
                let n = p
                    .as_i64()
                    .or_else(|| p.as_u64().map(|u| u as i64))
                    .or_else(|| p.as_f64().map(|f| f.round() as i64));
                if let Some(n) = n {
                    o.insert("price".into(), json!(n));
                }
            }
            Value::Object(o)
        })
        .collect();
    Value::Array(out)
}

fn send_go(go_tx: &GoTx, v: Value) -> Result<()> {
    let _ = go_tx.send(v);
    Ok(())
}

async fn send_fleet(state: &Arc<Mutex<OrchState>>, go_tx: &GoTx) -> Result<()> {
    let (types, roster, owner_json) = {
        let g = state.lock().await;
        let mut types = Vec::new();
        let mut roster = Vec::new();
        for rt in g.bots.values() {
            if !types.contains(&rt.cfg.go_type) {
                types.push(rt.cfg.go_type.clone());
            }
            roster.push(json!({
                "username": rt.cfg.username,
                "anarchy": rt.cfg.anarchy,
                "go_type": rt.cfg.go_type,
                "success": rt.success,
                "banned": rt.banned,
            }));
        }
        let owner_json: Option<Value> = None;
        (types, roster, owner_json)
    };
    send_go(
        go_tx,
        json!({ "action": "fleet", "types": types, "bots": roster, "owner": owner_json }),
    )
}

/// Ники из `bots/{an}b.json` прямо сейчас (файл перечитывается при каждом presence).
/// Go по этому списку решает, какие баны/проверки показывать; `None` — файл не прочитался,
/// Go оставит прошлый список.
fn read_config_bots(bots_path: &Path) -> Option<Vec<Value>> {
    let raw = std::fs::read_to_string(bots_path).ok()?;
    let list: Vec<BotJson> = serde_json::from_str(&raw).ok()?;
    Some(
        list.iter()
            .map(|b| json!({ "username": b.username, "anarchy": b.anarchy }))
            .collect(),
    )
}

async fn push_presence(state: &Arc<Mutex<OrchState>>, _go_tx: &GoTx) -> Result<()> {
    let payload = {
        let mut g = state.lock().await;
        let config_bots = read_config_bots(&g.bots_path);
        let config_anarchy = g.bots.values().next().map(|rt| rt.cfg.anarchy);
        let mut items: HashMap<String, u32> = HashMap::new();
        let mut inventory: HashMap<String, u32> = HashMap::new();
        let mut active_types = Vec::new();
        let mut bots_per_type: HashMap<String, u32> = HashMap::new();
        // слотов АХ по go-типу (сумма по тем же ботам, что в bots_per_type): 5 на бота, 10 с арендой
        let mut slots_per_type: HashMap<String, u32> = HashMap::new();
        let mut banned = Vec::new();
        let mut auth_faults = Vec::new();
        let mut roster = Vec::new();
        let mut proxies: Vec<Value> = Vec::new();
        let mut pricing_n = 0u32;
        let mut live_n = 0u32;
        let mut hub_n = 0u32;
        let mut unstable_n = 0u32;
        let now_ms = schedule::now_unix_ms();
        let mut scheduled_off = 0u32;
        let mut slot_rows: Vec<Value> = Vec::new();
        for (idx, _) in g.bot_order.iter().enumerate() {
            if !g.schedule.is_active(idx, now_ms) {
                scheduled_off += 1;
            }
        }

        let worker_alive: HashMap<String, bool> = g
            .bots
            .keys()
            .map(|u| (u.clone(), g.workers.contains_key(u)))
            .collect();

        for rt in g.bots.values_mut() {
            let live = worker_alive.get(&rt.cfg.username).copied().unwrap_or(false) && rt.success;
            if live {
                live_n += 1;
            }
            let has_stock = bot_has_pricing_stock(rt);
            let unstable = rt
                .unstable_until
                .map(|t| t > std::time::Instant::now())
                .unwrap_or(false);
            if unstable {
                unstable_n += 1;
            }
            if live && !rt.on_anarchy {
                hub_n += 1;
            }
            {
                let (mut ah, mut inv) = (HashMap::new(), HashMap::new());
                accumulate_presence_counts(&rt.items, &mut ah);
                accumulate_presence_counts(&rt.inventory, &mut inv);
                slot_rows.push(json!({
                    "bot": rt.cfg.username,
                    "anarchy": rt.cfg.anarchy,
                    "live": live,
                    "on_anarchy": rt.on_anarchy,
                    "on_ah": ah.values().sum::<u32>(),
                    "inv": inv.values().sum::<u32>(),
                }));
            }
            let include_stock = live && (rt.on_anarchy || has_stock);
            let count_as_bot = live && rt.on_anarchy && !unstable;
            if include_stock {
                accumulate_presence_counts(&rt.items, &mut items);
                accumulate_presence_counts(&rt.inventory, &mut inventory);
            }
            if count_as_bot {
                pricing_n += 1;
                if !active_types.contains(&rt.cfg.go_type) {
                    active_types.push(rt.cfg.go_type.clone());
                }
                *bots_per_type.entry(rt.cfg.go_type.clone()).or_insert(0) += 1;
                *slots_per_type.entry(rt.cfg.go_type.clone()).or_insert(0) += slots_for(rt.rent_until_ms, now_ms);
            }
            if rt.banned {
                banned.push(json!({
                    "username": rt.cfg.username,
                    "anarchy": rt.cfg.anarchy,
                    "go_type": rt.cfg.go_type,
                    "banned_at": rt.banned_at,
                    "reason": rt.banned_reason,
                    "kind": "ban",
                    "ip": proxy_host_hint(&rt.proxy),
                }));
            }
            if let Some(kind) = &rt.auth_fault {
                auth_faults.push(json!({
                    "username": rt.cfg.username,
                    "anarchy": rt.cfg.anarchy,
                    "go_type": rt.cfg.go_type,
                    "kind": kind,
                    "at": rt.auth_fault_at,
                    "reason": rt.auth_fault_reason,
                    "ip": proxy_host_hint(&rt.proxy),
                }));
            }
            // Счётчики по товарам в виде карты «товар → штук»: rt.items/inventory бывают списком или картой.
            let (mut ah_counts, mut inv_counts) = (HashMap::new(), HashMap::new());
            accumulate_presence_counts(&rt.items, &mut ah_counts);
            accumulate_presence_counts(&rt.inventory, &mut inv_counts);
            if !rt.proxy.is_empty() {
                // только хост: логин и пароль прокси Go не нужны
                proxies.push(json!({ "slot": rt.cfg.username, "kind": "bot", "ip": proxy_host_hint(&rt.proxy) }));
            }
            roster.push(json!({
                "username": rt.cfg.username,
                "anarchy": rt.cfg.anarchy,
                "go_type": rt.cfg.go_type,
                "success": rt.success,
                "on_anarchy": rt.on_anarchy,
                "banned": rt.banned,
                "unstable": unstable,
                "live": live,
                // Go считает по ним, кто из ботов реально может купить (деньги, слоты).
                "balance": rt.balance,
                "balance_age_s": rt.balance_at.map(|t| t.elapsed().as_secs()),
                // слотов АХ у этого бота (5, а пока действует аренда — 10) и до какого момента аренда
                "slots": slots_for(rt.rent_until_ms, now_ms),
                "rent_until_ms": rt.rent_until_ms,
                "items": ah_counts,
                "inventory": inv_counts,
            }));
        }

        log_bot_slots(&g.bots_path, &slot_rows);

        let owners_presence: Vec<Value> = g
            .treasurer_nick
            .as_ref()
            .map(|t| vec![json!({ "anarchy": 0, "username": t })])
            .unwrap_or_default();

        if active_types.is_empty() {
            warn!(
                "presence → active_types=[] live={live_n} hub={hub_n} unstable={unstable_n} pricing={pricing_n} scheduled_off={scheduled_off} (Go на паузе)"
            );
        } else {
            info!(
                "presence → active_types={active_types:?} pricing={pricing_n} live={live_n} hub={hub_n} unstable={unstable_n} scheduled_off={scheduled_off}"
            );
        }

        json!({
            "action": "presence",
            "items": items,
            "inventory": inventory,
            "active_types": active_types,
            "bots_per_type": bots_per_type,
            "slots_per_type": slots_per_type,
            "banned": banned,
            "auth_faults": auth_faults,
            "clan_owners": owners_presence,
            "proxies": proxies,
            "bots": roster,
            "config_bots": config_bots,
            "config_anarchy": config_anarchy,
            "owner": Value::Null,
            "treasurer": g.treasurer_nick,
            "fleet_stats": {
                "live": live_n,
                "pricing": pricing_n,
                "hub": hub_n,
                "unstable": unstable_n,
                "scheduled_off": scheduled_off,
            },
        })
    };
    send_go(_go_tx, payload)
}

fn note_pricing_edge(rt: &mut BotRuntime, ok: bool) {
    if rt.last_pricing_ok == ok {
        return;
    }
    rt.last_pricing_ok = ok;
    let now = std::time::Instant::now();
    rt.pricing_edges.push_back(now);
    let window = std::time::Duration::from_secs(15 * 60);
    while rt
        .pricing_edges
        .front()
        .map(|t| now.duration_since(*t) > window)
        .unwrap_or(false)
    {
        rt.pricing_edges.pop_front();
    }
    if rt.pricing_edges.len() >= 6 {
        rt.unstable_until = Some(now + std::time::Duration::from_secs(5 * 60));
        pretty_log::line(
            &rt.cfg.username,
            LineKind::Warn,
            &format!(
                "unstable: {} переключений за 15м → 5м вне pricing",
                rt.pricing_edges.len()
            ),
        );
    }
}

/// Раз в 30 секунд дописывает в `bot-slots.jsonl` (рядом с `bots/`) строку на каждого бота:
/// сколько у него лотов на АХ (`on_ah`, макс. 5) и предметов в инвентаре (`inv`). Нужно, чтобы
/// понять, бывает ли перекос: у одних АХ пуст, а другие забивают инвентарь.
fn log_bot_slots(bots_path: &Path, rows: &[Value]) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if rows.is_empty() {
        return;
    }
    {
        let Ok(mut last) = LAST.lock() else { return };
        if last.map_or(false, |t| t.elapsed() < Duration::from_secs(30)) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let ts = schedule::now_unix_ms() / 1000;
    let path = bots_path
        .parent()
        .unwrap_or(Path::new("."))
        .parent()
        .unwrap_or(Path::new("."))
        .join("bot-slots.jsonl");
    let mut out = String::new();
    for r in rows {
        let mut v = r.clone();
        v["ts"] = json!(ts);
        out.push_str(&v.to_string());
        out.push('\n');
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(out.as_bytes());
    }
}

fn accumulate_presence_counts(raw: &Value, out: &mut HashMap<String, u32>) {
    match raw {
        Value::Array(arr) => {
            for el in arr {
                match el {
                    Value::String(id) if !id.is_empty() => {
                        *out.entry(id.clone()).or_insert(0) += 1;
                    }
                    Value::Object(o) => {
                        if let Some(id) = o
                            .get("id")
                            .or_else(|| o.get("go_type"))
                            .and_then(|v| v.as_str())
                        {
                            let n = o.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                            *out.entry(id.to_string()).or_insert(0) += n.max(1);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(n) = v.as_u64() {
                    if n > 0 {
                        *out.entry(k.clone()).or_insert(0) += n as u32;
                    }
                } else if v.as_str().is_some() {
                    *out.entry(k.clone()).or_insert(0) += 1;
                }
            }
        }
        _ => {}
    }
}

fn bot_has_pricing_stock(rt: &BotRuntime) -> bool {
    match &rt.items {
        Value::Array(a) if !a.is_empty() => return true,
        Value::Object(o) if o.values().any(|v| v.as_u64().unwrap_or(0) > 0) => return true,
        _ => {}
    }
    match &rt.inventory {
        Value::Array(a) if !a.is_empty() => return true,
        Value::Object(o) if !o.is_empty() => return true,
        _ => {}
    }
    false
}

// ============ go ws ============

async fn go_ws_loop(
    state: Arc<Mutex<OrchState>>,
    go_tx: GoTx,
    mut go_rx: mpsc::UnboundedReceiver<Value>,
    url: String,
) {
    loop {
        info!("Go WS → {url}");
        match connect_async(&url).await {
            Ok((ws, _)) => {
                let (mut sink, mut stream) = ws.split();
                info!("Go WS open");
                let _ = send_fleet(&state, &go_tx).await;
                let _ = push_presence(&state, &go_tx).await;
                let _ = go_tx.send(json!({ "action": "info" }));

                loop {
                    tokio::select! {
                        msg = stream.next() => {
                            match msg {
                                Some(Ok(Message::Text(t))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                        if let Err(e) = on_go_message(&state, &go_tx, v).await {
                                            warn!("go msg: {e:#}");
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                Some(Err(e)) => {
                                    error!("go ws: {e}");
                                    break;
                                }
                                _ => {}
                            }
                        }
                        out = go_rx.recv() => {
                            match out {
                                Some(v) => {
                                    if sink.send(Message::Text(v.to_string().into())).await.is_err() {
                                        break;
                                    }
                                }
                                None => return,
                            }
                        }
                    }
                }
            }
            Err(e) => error!("go connect: {e}"),
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn on_go_message(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    v: Value,
) -> Result<()> {
    if v.get("prices").is_some() {
        let merged = merge_go_catalog_prices(&v);
        info!("Go prices → {} позиций в каталоге", merged.len());
        if merged.is_empty() {
            return Ok(());
        }
        let mut g = state.lock().await;
        g.last_catalog = merged.clone();
        let targets: Vec<(String, String)> = g
            .workers
            .keys()
            .filter_map(|name| {
                g.bots.get(name).map(|b| (name.clone(), b.cfg.go_type.clone()))
            })
            .collect();
        for (name, go_type) in targets {
            let bot_items: Vec<Value> = merged
                .iter()
                .filter(|it| {
                    let ty = it.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    catalog_type_matches_go(ty, &go_type, name)
                })
                .cloned()
                .collect();
            let cmd = OrchCmd::Update { data: bot_items, all: merged.clone() };
            let line = serde_json::to_string(&cmd)? + "\n";
            if let Some(wh) = g.workers.get_mut(&name) {
                if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
                    warn!("stdin {name}: {e}");
                }
            }
        }
    } else if v.get("action").and_then(|a| a.as_str()) == Some("claim_result") {
        // Go решил, кто владеет лотом: его ответ главнее локальной догадки.
        let uuid = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("");
        let owner = v.get("owner").and_then(|x| x.as_str()).unwrap_or("");
        if !uuid.is_empty() && !owner.is_empty() {
            let merged = {
                let mut g = state.lock().await;
                let mut list = g.items_buying.as_array().cloned().unwrap_or_default();
                list.retain(|raw| !matches!(normalize_buying_entry(raw), Some((u, _)) if u == uuid));
                list.push(json!({ "uuid": uuid, "username": owner }));
                g.items_buying = Value::Array(list);
                g.items_buying.clone()
            };
            broadcast_cmd(state, &OrchCmd::ItemsBuying { data: merged }).await?;
        }
    } else if v.get("action").and_then(|a| a.as_str()) == Some("json_update") {
        if let Some(data) = v.get("data") {
            let merged = {
                let mut g = state.lock().await;
                let merged = merge_go_json_update(&g.items_buying, data);
                g.items_buying = merged.clone();
                merged
            };
            let cmd = OrchCmd::ItemsBuying { data: merged };
            broadcast_cmd(state, &cmd).await?;
        }
    } else if v.get("action").and_then(|a| a.as_str()) == Some("capital_plan")
        || v.get("type").and_then(|a| a.as_str()) == Some("capital_plan")
    {
        let keep = v.get("keepSum").or_else(|| v.get("keep_sum")).and_then(|x| x.as_f64());
        let max_buy = v.get("maxBuyPrice").or_else(|| v.get("max_buy_price")).and_then(|x| x.as_f64());
        let cmd = OrchCmd::CapitalPlan { keep_sum: keep, max_buy_price: max_buy };
        broadcast_cmd(state, &cmd).await?;
    }
    Ok(())
}

fn merge_go_catalog_prices(v: &Value) -> Vec<Value> {
    let prices = match v.get("prices").and_then(|p| p.as_object()) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let catalog = match v.get("catalog").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in catalog {
        let id = item.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let sell = prices.get(id).and_then(|p| {
            p.as_f64()
                .or_else(|| p.as_i64().map(|i| i as f64))
                .or_else(|| p.as_u64().map(|u| u as f64))
        });
        let Some(ps) = sell else { continue };
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("priceSell".into(), json!(ps));
        }
        out.push(obj);
    }
    out
}

fn catalog_type_matches_go(catalog_type: &str, go_type: &str, catalog_name: &str) -> bool {
    if go_type.is_empty() {
        return true;
    }
    if catalog_type == go_type {
        return true;
    }
    const ARMOR: &str = "netherite_armor-1.21";
    const PIECES: &[(&str, &str)] = &[
        ("netherite_helmet-1.21", "netherite_helmet"),
        ("netherite_chestplate-1.21", "netherite_chestplate"),
        ("netherite_leggings-1.21", "netherite_leggings"),
        ("netherite_boots-1.21", "netherite_boots"),
    ];
    if go_type == ARMOR {
        if PIECES.iter().any(|(t, _)| *t == catalog_type) {
            return true;
        }
        if catalog_type == ARMOR {
            return catalog_name.is_empty()
                || PIECES.iter().any(|(_, n)| *n == catalog_name);
        }
        return false;
    }
    if catalog_type == ARMOR {
        return PIECES.iter().any(|(t, n)| *t == go_type && *n == catalog_name);
    }
    let a = catalog_type.split('-').next().unwrap_or(catalog_type);
    let b = go_type.split('-').next().unwrap_or(go_type);
    a == b
}

fn merge_go_json_update(local: &Value, go_uuids: &Value) -> Value {
    let go_arr = match go_uuids.as_array() {
        Some(a) => a,
        None => {
            return if local.is_array() { local.clone() } else { json!([]) };
        }
    };
    let prev = local.as_array().cloned().unwrap_or_default();
    let mut owner_by_uuid: HashMap<String, String> = HashMap::new();
    for raw in &prev {
        if let Some((u, Some(owner))) = normalize_buying_entry(raw) {
            owner_by_uuid.insert(u, owner);
        }
    }
    let mut in_go = std::collections::HashSet::new();
    let mut out = Vec::new();
    for uuid_v in go_arr {
        let u = match uuid_v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            _ => continue,
        };
        if u.is_empty() {
            continue;
        }
        in_go.insert(u.clone());
        let go_owner = uuid_v
            .get("username")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(owner) = go_owner {
            out.push(json!({ "uuid": u, "username": owner }));
        } else if let Some(owner) = owner_by_uuid.get(&u) {
            out.push(json!({ "uuid": u, "username": owner }));
        } else {
            out.push(json!({ "uuid": u, "username": Value::Null }));
        }
    }
    // Go — единственный источник правды по заявкам (первый заявивший, срок жизни, снятие).
    // Локальные записи, которых в его списке нет, истекли или сняты: не держим их вечно.
    let _ = in_go;
    Value::Array(out)
}

fn normalize_buying_entry(raw: &Value) -> Option<(String, Option<String>)> {
    match raw {
        Value::String(s) if !s.is_empty() => Some((s.clone(), None)),
        Value::Object(o) => {
            let u = o.get("uuid").and_then(|v| v.as_str())?.to_string();
            if u.is_empty() {
                return None;
            }
            let owner = o.get("username").and_then(|v| v.as_str()).map(|s| s.to_string());
            Some((u, owner))
        }
        _ => None,
    }
}

fn top_two_sell_keep(catalog: &[Value]) -> Option<f64> {
    let mut prices: Vec<f64> = Vec::new();
    for entry in catalog {
        if let Some(p) = entry.get("priceSell").and_then(|v| v.as_f64()) {
            if p.is_finite() && p > 0.0 {
                prices.push(p);
            }
        }
    }
    if prices.is_empty() {
        return None;
    }
    prices.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    if prices.len() == 1 {
        return Some(prices[0].floor());
    }
    Some((prices[0] + prices[1]).floor())
}

fn top_sell_price(catalog: &[Value]) -> Option<f64> {
    let mut best = 0.0;
    for entry in catalog {
        if let Some(p) = entry.get("priceSell").and_then(|v| v.as_f64()) {
            if p.is_finite() && p > best {
                best = p;
            }
        }
    }
    if best > 0.0 { Some(best) } else { None }
}

async fn push_capital_plan_to_bots(state: &Arc<Mutex<OrchState>>) -> Result<()> {
    let plans: Vec<(String, Option<f64>, Option<f64>)> = {
        let g = state.lock().await;
        let mut out = Vec::new();
        for name in g.workers.keys() {
            let Some(bot) = g.bots.get(name) else { continue };
            let bot_cat: Vec<Value> = g
                .last_catalog
                .iter()
                .filter(|it| {
                    let ty = it.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let n = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    catalog_type_matches_go(ty, &bot.cfg.go_type, n)
                })
                .cloned()
                .collect();
            let keep = top_two_sell_keep(&bot_cat);
            let max_buy = top_sell_price(&bot_cat).map(|t| (t * 0.9).floor());
            out.push((name.clone(), keep, max_buy));
        }
        out
    };
    for (name, keep, max_buy) in plans {
        let cmd = OrchCmd::CapitalPlan { keep_sum: keep, max_buy_price: max_buy };
        let line = serde_json::to_string(&cmd)? + "\n";
        let mut g = state.lock().await;
        if let Some(wh) = g.workers.get_mut(&name) {
            if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
                warn!("stdin {name}: {e}");
            }
        }
    }
    Ok(())
}

async fn broadcast_cmd(state: &Arc<Mutex<OrchState>>, cmd: &OrchCmd) -> Result<()> {
    let line = serde_json::to_string(cmd)? + "\n";
    let mut g = state.lock().await;
    for (name, wh) in g.workers.iter_mut() {
        if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
            warn!("stdin {name}: {e}");
        }
    }
    Ok(())
}

async fn send_cmd_to(state: &Arc<Mutex<OrchState>>, username: &str, cmd: &OrchCmd) -> Result<()> {
    let line = serde_json::to_string(cmd)? + "\n";
    let mut g = state.lock().await;
    if let Some(wh) = g.workers.get_mut(username) {
        wh.stdin.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn format_money(v: f64) -> String {
    let v = v.abs();
    if v >= 1_000_000.0 {
        let kk = v / 1_000_000.0;
        if (kk - kk.round()).abs() < 0.05 {
            format!("{}kk", kk.round() as i64)
        } else {
            format!("{kk:.1}kk")
        }
    } else if v >= 10_000.0 {
        format!("{}k", (v / 1000.0).round() as i64)
    } else if v >= 1000.0 {
        let k = v / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as i64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        format!("{}", v.round() as i64)
    }
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn auth_fault_kind_from_exit(code: Option<i32>) -> Option<&'static str> {
    match code {
        Some(71) => Some("bad_password"),
        Some(72) => Some("proxy_error"),
        _ => None,
    }
}

fn proxy_host_hint(proxy: &str) -> String {
    if let Some(at) = proxy.rfind('@') {
        return proxy[at + 1..].to_string();
    }
    proxy
        .trim_start_matches("socks5://")
        .trim_start_matches("socks://")
        .to_string()
}
#[cfg(test)]
mod ban_memory_tests {
    use super::*;

    fn entry(anarchy_in_reason: Option<u32>) -> Value {
        match anarchy_in_reason {
            Some(a) => json!({"username": "x", "reason": format!("кикнуты при подключении к серверу Anarchy{a}: ВЫ ЗАБАНЕНЫ")}),
            None => json!({"username": "x", "reason": "непонятно"}),
        }
    }

    fn run(map: HashMap<String, Value>, current: &[&str], my: u32) -> HashMap<String, Value> {
        let dir = std::env::temp_dir().join(format!("bm-{}-{my}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("banned-memory.json");
        let cur = current.iter().map(|s| s.to_string()).collect();
        let out = prune_remembered_bans(&path, map, cur, my);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn only_this_anarchys_renamed_bots_are_removed() {
        let mut map = HashMap::new();
        map.insert("old502".to_string(), entry(Some(502))); // ник сменили → убрать
        map.insert("kept502".to_string(), entry(Some(502))); // в конфиге и забанен → оставить
        map.insert("other503".to_string(), entry(Some(503))); // чужая анархия → не трогать
        map.insert("unknown".to_string(), entry(None)); // анархию не определили → не трогать
        let out = run(map, &["kept502", "new502"], 502);
        assert!(!out.contains_key("old502"));
        assert!(out.contains_key("kept502"));
        assert!(out.contains_key("other503"));
        assert!(out.contains_key("unknown"));
    }

    #[test]
    fn explicit_anarchy_field_wins_over_reason() {
        let mut e = entry(Some(504));
        e["anarchy"] = json!(502);
        assert_eq!(ban_anarchy(&e), Some(502));
        assert_eq!(ban_anarchy(&entry(Some(506))), Some(506));
        assert_eq!(ban_anarchy(&entry(None)), None);
    }
}

#[cfg(test)]
mod claims_merge_tests {
    use super::*;

    #[test]
    fn go_owner_wins_and_stale_local_claims_are_dropped() {
        let local = json!([
            {"uuid": "u1", "username": "botB"},   // локально думали botB
            {"uuid": "stale", "username": "botC"} // Go о ней уже не знает (истекла)
        ]);
        let go = json!([{"uuid": "u1", "username": "botA"}]);
        let out = merge_go_json_update(&local, &go);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["uuid"], "u1");
        assert_eq!(arr[0]["username"], "botA");
    }
}

#[cfg(test)]
mod rent_slots_tests {
    use super::*;

    #[test]
    fn slots_follow_rent() {
        let t = 10_000u64;
        assert_eq!(slots_for(0, t), 5);
        assert_eq!(slots_for(t, t), 5); // аренда кончилась ровно сейчас
        assert_eq!(slots_for(t + 1, t), 10);
    }
}

#[cfg(test)]
mod bindings_file_tests {
    use super::*;

    #[test]
    fn save_binding_keeps_other_orchestrators_entries() {
        let dir = std::env::temp_dir().join(format!("bind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bindings.json");
        std::fs::write(&p, r#"{"a502": true, "b503": true}"#).unwrap();
        // оркестратор 508 загрузил файл раньше, потом 509 дописал свою запись
        save_binding(&p, "c509", true);
        // 508 пишет свою — чужие (в т.ч. новая c509) не пропадают
        save_binding(&p, "d508", false);
        let m = load_bindings(&p);
        assert_eq!(m.get("a502"), Some(&true));
        assert_eq!(m.get("c509"), Some(&true));
        assert_eq!(m.get("d508"), Some(&false));
        save_binding(&p, "d508", true);
        assert_eq!(load_bindings(&p).get("d508"), Some(&true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
