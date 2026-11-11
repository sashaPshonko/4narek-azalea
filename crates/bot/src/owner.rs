//! Owner-бот: заходит по требованию, раздаёт деньги, выходит.
//! Не 24/7. Триггеры захода решает орк (см. orch) и передаёт в `boot.owner_reason`:
//!   - MyNickSeen  → любой бот увидел myNick (в табе или в чате) → owner: /balance →
//!                   redistribute (пополнить ботов до KEEP_SUM) →
//!                   остаток > KEEP_SUM → /pay myNick ×2 → выход.
//!   - Redistribute → есть активный farm-бот с balance < 13kk → redistribute
//!                    + myNick-слив излишка (если остался).
//!   - Collect      → есть активный farm-бот с balance > 20kk → подождать
//!                    реальный перевод (по чату `[$] От игрока ... получено`)
//!                    до HIGH_BALANCE_WAIT (5 минут) + redistribute + myNick-слив.
//!   - Nothing      → на всякий.
//!
//! `redistribute` раздаёт ботам **всё, что есть у овнера** (`available = owner_bal`),
//! не оставляя себе `KEEP_SUM`. `KEEP_SUM` применяется только в `maybe_pay_my_nick`:
//! перед сливом на myNick овнер оставляет себе `KEEP_SUM`.
//!
//! Баланс, пришедший как `-1.0`, значит «неизвестно» (farm-бот ещё не сделал
//! `/balance`). Такой бот **резервируется** как будто у него 0: овнер считает
//! его в `total_needed`, но **не платит** ему, пока баланс не станет известен.
//!
//! **Проверка таба перед каждым `/pay`:** и myNick, и ники farm-ботов
//! должны быть в табе. Если получатель не онлайн — `/pay` не шлём
//! (иначе спам `Указанный игрок не найден` и палево).
//!
//! **Минимум `/balance`:** один в начале захода, один внутри `redistribute`,
//! один в `maybe_pay_my_nick` (только если myNick в табе). Не 3–4 подряд.
//!
//! Анти-АФК — только перед отправкой команд (`do_afk`), не отдельным тикером.
//! В `collect`-ожидании — `un_afk` каждые 5–10с (рандом), иначе овнер уйдёт
//! в AFK и `/balance` после ожидания не пройдёт.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, OwnerReason, WorkerMsg};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::afk;
use crate::boot::BalancesSnapshotReq;
use crate::cmd;
use crate::state::BotState;

pub const KEEP_SUM_BASE: f64 = 15_000_000.0;
pub fn keep_sum() -> f64 { ipc::balance::k(KEEP_SUM_BASE) }
pub const MIN_BOT_BALANCE_BASE: f64 = 13_000_000.0;
pub fn min_bot_balance() -> f64 { ipc::balance::k(MIN_BOT_BALANCE_BASE) }
pub const HIGH_BALANCE_TRIGGER_BASE: f64 = 20_000_000.0;
pub fn high_balance_trigger() -> f64 { ipc::balance::k(HIGH_BALANCE_TRIGGER_BASE) }
const PAY_SETTLE: Duration = Duration::from_millis(1500);
const BALANCE_WAIT: Duration = Duration::from_secs(5);
const MIN_PAY_AMOUNT: f64 = 200_000.0;
/// Сколько ждём реальный перевод от богатого бота (Collect).
const HIGH_BALANCE_WAIT: Duration = Duration::from_secs(60);
/// Как часто проверяем `state.last_payment` во время ожидания.
const PAYMENT_POLL: Duration = Duration::from_millis(500);
const MY_NICK_WAIT: Duration = Duration::from_secs(10);
const BALANCES_WAIT: Duration = Duration::from_secs(5);
const BALANCE_UNKNOWN: f64 = -1.0;

/// Рандомный интервал между `un_afk` во время ожидания перевода: 5–10с.
fn afk_poll_interval() -> Duration {
    let ms = 5_000 + (rand::random::<u64>() % 5_001);
    Duration::from_millis(ms)
}

#[derive(Debug, Clone)]
pub struct PayRequest {
    pub to: String,
    pub amount: i64,
    pub req_id: u64,
}

/// Снимок таба: онлайн-ники (lowercase).
fn tab_snapshot(state: &Arc<Mutex<BotState>>) -> HashSet<String> {
    state
        .lock()
        .tab_names
        .iter()
        .map(|n| n.to_lowercase())
        .collect()
}

/// Ник в табе? Если таб пуст — считаем онлайн, чтобы не блокировать
/// первый `/pay`. После первого `PlayerInfoUpdate` set непустой.
fn is_tab_online(tab: &HashSet<String>, nick: &str) -> bool {
    if tab.is_empty() {
        return true;
    }
    tab.contains(&nick.to_lowercase())
}

pub async fn run_once(
    bot: Client,
    boot: BotBoot,
    state: Arc<Mutex<BotState>>,
    mut balances_rx: mpsc::UnboundedReceiver<BalancesSnapshotReq>,
    mut my_nick_rx: mpsc::UnboundedReceiver<String>,
) {
    let reason = boot.owner_reason.unwrap_or(OwnerReason::Redistribute);
    info!(
        "owner::run_once старт · nick={} an={} reason={:?}",
        boot.username, boot.anarchy, reason
    );

    // Убираем ранний /balance: он шёл ДО проверки таба, даже если
    // получатель не онлайн. Первый /balance теперь — внутри ветки.
    let mut paid = 0u32;
    match reason {
        OwnerReason::MyNickSeen => {
            let my_nick = match tokio::time::timeout(MY_NICK_WAIT, my_nick_rx.recv()).await {
                Ok(Some(nick)) => nick,
                Ok(None) => {
                    warn!("owner: my_nick_seen — канал myNick закрыт");
                    boot.my_nick.clone().unwrap_or_default()
                }
                Err(_) => {
                    warn!("owner: my_nick_seen — myNick не пришёл, берём из boot");
                    boot.my_nick.clone().unwrap_or_default()
                }
            };
            if my_nick.is_empty() {
                warn!("owner: my_nick_seen — нет myNick, выходим");
                finish(&bot, &boot, "my_nick_seen_no_nick", 0).await;
                return;
            }

            // Проверка таба ДО /balance: если myNick не онлайн — выходим,
            // не делая ни одной команды.
            let tab = tab_snapshot(&state);
            if !is_tab_online(&tab, &my_nick) {
                info!(
                    "owner: my_nick_seen, но {my_nick} не в табе — выходим без /balance и /pay"
                );
                finish(&bot, &boot, "my_nick_seen_offline", 0).await;
                return;
            }

            // Один /balance перед redistribute (внутри redistribute своего
            // /balance нет — owner_bal берётся из state.balance).
            let _ = refresh_owner_balance(&bot, &state).await;

            let balances = wait_balances(&mut balances_rx).await;
            info!(
                "owner: my_nick_seen — снимок {} ботов",
                balances.len()
            );

            paid = redistribute(&bot, &state, &balances).await;
            maybe_pay_my_nick(&bot, &boot, &state).await;
            finish(&bot, &boot, "my_nick_seen", paid).await;
            return;
        }
        OwnerReason::Collect => {
            let mut balances = wait_balances(&mut balances_rx).await;
            let high = balances.iter().any(|(_, b)| *b > high_balance_trigger());
            if high {
                info!(
                    "owner: collect — есть бот > {}, жду реальный перевод (до {}с)",
                    high_balance_trigger(),
                    HIGH_BALANCE_WAIT.as_secs()
                );
                state.lock().last_payment = None;
                let deadline = Instant::now() + HIGH_BALANCE_WAIT;
                let mut got = false;
                let mut afk_next = Instant::now() + afk_poll_interval();
                while Instant::now() < deadline {
                    if let Some((from, amount, _at)) = state.lock().last_payment.clone() {
                        info!(
                            "owner: collect — получен перевод от {from}: {}",
                            crate::money::money(amount)
                        );
                        got = true;
                        break;
                    }
                    if Instant::now() >= afk_next {
                        afk::un_afk(&bot).await;
                        afk_next = Instant::now() + afk_poll_interval();
                    }
                    tokio::time::sleep(PAYMENT_POLL).await;
                }
                if !got {
                    warn!(
                        "owner: collect — перевод не пришёл за {}с",
                        HIGH_BALANCE_WAIT.as_secs()
                    );
                }
                while let Ok(snap) = balances_rx.try_recv() {
                    balances = snap.balances;
                }
            }
            // Один /balance перед redistribute.
            let _ = refresh_owner_balance(&bot, &state).await;
            paid = redistribute(&bot, &state, &balances).await;
            maybe_pay_my_nick(&bot, &boot, &state).await;
            finish(&bot, &boot, "collect", paid).await;
            return;
        }
        OwnerReason::Redistribute => {
            let balances = wait_balances(&mut balances_rx).await;
            info!("owner: redistribute — снимок {} ботов", balances.len());
            // Один /balance перед redistribute.
            let _ = refresh_owner_balance(&bot, &state).await;
            paid = redistribute(&bot, &state, &balances).await;
            maybe_pay_my_nick(&bot, &boot, &state).await;
            finish(&bot, &boot, "redistribute", paid).await;
            return;
        }
        OwnerReason::Nothing => {
            info!("owner: reason=Nothing — выходим");
            finish(&bot, &boot, "nothing", 0).await;
            return;
        }
    }
}

async fn wait_balances(
    rx: &mut mpsc::UnboundedReceiver<BalancesSnapshotReq>,
) -> Vec<(String, f64)> {
    let mut latest: Option<Vec<(String, f64)>> = None;
    while let Ok(snap) = rx.try_recv() {
        latest = Some(snap.balances);
    }
    if let Some(b) = latest {
        return b;
    }
    match tokio::time::timeout(BALANCES_WAIT, rx.recv()).await {
        Ok(Some(snap)) => snap.balances,
        _ => Vec::new(),
    }
}

async fn finish(bot: &Client, boot: &BotBoot, reason: &str, paid: u32) {
    emit(&WorkerMsg {
        name: "owner_done".into(),
        username: Some(boot.username.clone()),
        data: Some(json!({ "reason": reason, "paid": paid })),
        ..crate::empty_msg()
    });
    info!("owner: owner_done reason={reason} paid={paid} → выход");
    let _ = bot.disconnect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::process::exit(0);
}

/// Запрашивает `/balance` с `un_afk` перед ним. Возвращает `Some(bal)` при
/// успехе и `None`, если за `BALANCE_WAIT` баланс не пришёл (AFK / лаг / кик).
async fn refresh_owner_balance(bot: &Client, state: &Arc<Mutex<BotState>>) -> Option<f64> {
    state.lock().balance = None;
    afk::un_afk(bot).await;
    crate::delay::rnd("BASE_DELAY").await;
    cmd::say(bot, "/balance");
    let deadline = Instant::now() + BALANCE_WAIT;
    while Instant::now() < deadline {
        if state.lock().balance.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let b = state.lock().balance;
    match b {
        Some(v) => info!("owner: /balance = {}", crate::money::money(v)),
        None => warn!("owner: /balance не ответил за {:?}", BALANCE_WAIT),
    }
    b
}

/// `/pay myNick` только если myNick в табе. `/balance` **не** запрашиваем —
/// используем `state.balance` (обновлён в `run_once` / `redistribute`).
async fn maybe_pay_my_nick(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    let Some(my_nick) = boot.my_nick.clone() else {
        return;
    };

    let tab = tab_snapshot(state);
    if !is_tab_online(&tab, &my_nick) {
        info!(
            "owner: myNick {my_nick} не в табе — пропуск /pay (не палимся)"
        );
        return;
    }

    // Берём последний balance из state — /balance уже был в run_once/redistribute.
    let bal = match state.lock().balance {
        Some(b) => b,
        None => {
            warn!("owner: myNick {my_nick} — balance неизвестен, пропуск");
            return;
        }
    };
    let to_send = (bal - keep_sum()).floor();
    if to_send <= 0.0 {
        info!("owner: myNick {my_nick} — излишка нет (bal={bal})");
        return;
    }
    let amount = to_send as i64;
    do_afk(bot, state).await;
    info!("owner → myNick {my_nick}: /pay {amount} ×2 (в табе)");
    do_pay_twice(bot, &my_nick, amount).await;
}

async fn do_afk(bot: &Client, state: &Arc<Mutex<BotState>>) {
    if state.lock().shutdown || state.lock().banned {
        return;
    }
    afk::un_afk(bot).await;
    let mut st = state.lock();
    if st.afk {
        st.afk = false;
    }
}

/// Два `/pay` — **оставлено как было**. Второй нужен, если FunTime
/// просит подтверждения (`Необходимо подтверждение`).
async fn do_pay_twice(bot: &Client, to: &str, amount: i64) -> bool {
    crate::delay::rnd("BASE_DELAY").await;
    if !cmd::pay(bot, to, amount) {
        return false;
    }
    let ms = 500 + (rand::random::<u64>() % 501);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    if !cmd::pay(bot, to, amount) {
        return false;
    }
    tokio::time::sleep(PAY_SETTLE).await;
    true
}

/// Раздача ботам. `available = owner_bal` целиком — овнер **не** оставляет
/// себе KEEP_SUM при раздаче ботам. KEEP_SUM применяется только в
/// `maybe_pay_my_nick` (перед сливом на myNick).
///
/// **Перед каждым `/pay` проверяем, что ник получателя в табе.**
/// Если не в табе — пропускаем (не палимся).
///
/// `/balance` **здесь больше нет** — owner_bal берётся из `state.balance`,
/// который обновили в `run_once` один раз.
async fn redistribute(
    bot: &Client,
    state: &Arc<Mutex<BotState>>,
    balances: &[(String, f64)],
) -> u32 {
    if balances.is_empty() {
        return 0;
    }
    if state.lock().banned || state.lock().shutdown {
        return 0;
    }
    if !state.lock().on_anarchy {
        info!("owner: redistribution — не на анке, пропуск");
        return 0;
    }

    // Снимок таба: какие ники сейчас онлайн.
    let tab = tab_snapshot(state);
    if tab.is_empty() {
        info!("owner: redistribution — таб пуст, пропуск (не палимся)");
        return 0;
    }

    // owner_bal берём из state.balance (обновлён в run_once).
    let owner_bal = match state.lock().balance {
        Some(b) => b,
        None => {
            warn!("owner: redistribution — balance неизвестен, пропуск");
            return 0;
        }
    };

    let available = owner_bal.floor();
    if available < MIN_PAY_AMOUNT {
        info!(
            "owner: redistribution — available {} < {} , копим",
            available, MIN_PAY_AMOUNT
        );
        return 0;
    }

    let mut needs: Vec<(String, f64, bool)> = balances
        .iter()
        .filter_map(|(nick, bal)| {
            if (*bal - BALANCE_UNKNOWN).abs() < 0.5 {
                return Some((nick.clone(), keep_sum(), true));
            }
            if *bal >= min_bot_balance() {
                return None;
            }
            let shortfall = (keep_sum() - bal).floor();
            if shortfall > 0.0 {
                Some((nick.clone(), shortfall, false))
            } else {
                None
            }
        })
        .collect();
    needs.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.cmp(&b.2))
    });

    if needs.is_empty() {
        info!(
            "owner: redistribution — у всех >= {} — раздавать некому",
            min_bot_balance()
        );
        return 0;
    }

    // Отфильтровать ники, которых нет в табе: им не заплатим, только
    // зарезервируем (как unknown), чтобы не терять сумму.
    let mut missing_in_tab = 0u32;
    let mut payable_needs: Vec<(String, f64)> = Vec::new();
    let mut total_needed: f64 = 0.0;
    for (nick, shortfall, unknown) in &needs {
        if *unknown {
            total_needed += shortfall;
            continue;
        }
        if !is_tab_online(&tab, nick) {
            missing_in_tab += 1;
            continue;
        }
        total_needed += shortfall;
        payable_needs.push((nick.clone(), *shortfall));
    }
    if missing_in_tab > 0 {
        info!(
            "owner: redistribution — {} ник(ов) не в табе, пропуск /pay",
            missing_in_tab
        );
    }
    if payable_needs.is_empty() {
        info!("owner: redistribution — все получатели не в табе, стоп");
        return 0;
    }

    let n_unknown = needs.iter().filter(|(_, _, u)| *u).count();
    info!(
        "owner: redistribution — available={}, total_needed={}, payable={}, unknown={}, not_in_tab={}",
        available,
        total_needed,
        payable_needs.len(),
        n_unknown,
        missing_in_tab
    );

    let mut paid_count = 0u32;
    let mut remaining = available;

    if remaining >= total_needed {
        for (nick, shortfall) in &payable_needs {
            let amount = shortfall.floor() as i64;
            if amount < MIN_PAY_AMOUNT as i64 {
                continue;
            }
            // Ещё раз проверить таб перед самим /pay (мог выйти за время цикла).
            if !is_tab_online(&tab_snapshot(state), nick) {
                info!(
                    "owner: {nick} вышел из таба — пропуск /pay"
                );
                continue;
            }
            info!(
                "owner → {}: /pay {} ×2 (добить до KEEP_SUM, shortfall={}, remaining={})",
                nick, amount, shortfall, remaining
            );
            do_afk(bot, state).await;
            do_pay_twice(bot, nick, amount).await;
            remaining -= amount as f64;
            paid_count += 1;
        }
        info!(
            "owner: redistribution — раздал известным, remaining={} (резерв на {} неизв. + {} не в табе)",
            remaining, n_unknown, missing_in_tab
        );
        return paid_count;
    }

    // available < total_needed — выравнивание по per_bot.
    let mut known: Vec<(String, f64)> = payable_needs;

    while remaining >= MIN_PAY_AMOUNT && !known.is_empty() {
        let n = known.len() as f64;
        let per_bot = (remaining / n).floor();

        if per_bot < MIN_PAY_AMOUNT {
            let (nick, shortfall) = known.remove(0);
            let amount_f = shortfall.min(remaining).floor();
            if amount_f < MIN_PAY_AMOUNT {
                info!(
                    "owner: {} shortfall={}, remaining={} — платить нечего",
                    nick, shortfall, remaining
                );
                break;
            }
            let amount = amount_f as i64;
            if !is_tab_online(&tab_snapshot(state), &nick) {
                info!("owner: {nick} вышел из таба — пропуск /pay");
                break;
            }
            info!(
                "owner → {}: /pay {} ×2 [последний] (shortfall={}, remaining={})",
                nick, amount, shortfall, remaining
            );
            do_afk(bot, state).await;
            do_pay_twice(bot, &nick, amount).await;
            remaining -= amount_f;
            paid_count += 1;
            break;
        }

        let mut any_paid = false;
        let mut idx = known.len();
        while idx > 0 {
            idx -= 1;
            let (nick, shortfall) = known[idx].clone();
            let amount_f = shortfall.min(per_bot).floor();
            if amount_f < MIN_PAY_AMOUNT {
                known.remove(idx);
                continue;
            }
            if !is_tab_online(&tab_snapshot(state), &nick) {
                info!("owner: {nick} вышел из таба — пропуск /pay");
                known.remove(idx);
                continue;
            }
            let amount = amount_f as i64;
            info!(
                "owner → {}: /pay {} ×2 (per_bot={}, shortfall={}, remaining={})",
                nick, amount, per_bot, shortfall, remaining
            );
            do_afk(bot, state).await;
            do_pay_twice(bot, &nick, amount).await;
            remaining -= amount_f;
            let new_shortfall = shortfall - amount_f;
            if new_shortfall >= MIN_PAY_AMOUNT {
                known[idx].1 = new_shortfall;
            } else {
                known.remove(idx);
            }
            any_paid = true;
            paid_count += 1;
            if remaining < MIN_PAY_AMOUNT {
                break;
            }
        }
        if !any_paid {
            info!("owner: выравнивание — никто не получил, стоп");
            break;
        }
    }

    paid_count
}