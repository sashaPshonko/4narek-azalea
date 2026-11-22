//! Продажа: listing id 0–4 + /ah sell + clan после.
//! Задержки — `delay.json` / SELL_* как 4narek-old.
//!
//! **Казначей:** в конце `sell_pass` (после `/balance`) — если я казначей,
//! раздаю излишек (`bal - KEEP_SUM`) другим farm-ботам по
//! `state.balances_snapshot` (от орка через `OrchCmd::BalancesSnapshot`).
//! АХ-цикл **не** прерывается — раздача идёт в `sell_pass`, между продажами.
//!
//! **После раздачи** — остаток (`available - actually_sent`) отправляется
//! `my_nick` (если он в табе и остаток > `MY_NICK_MIN_PAY`). Это
//! восстанавливает логику старого `owner.rs::maybe_pay_my_nick`, но
//! с **точным** учётом реально отправленного (а не по `state.balance`,
//! который мог не обновиться). Так `my_nick` не получит больше, чем
//! реально осталось на казначее.
//!
//! **Все `/pay` — ×2.** FunTime требует подтверждения: первая команда
//! — запрос, вторая — подтверждение. Без второй сервер отвечает
//! «Необходимо подтверждение» и перевод не проходит. Это та же логика,
//! что в `flush_treasurer_loop` и `excess_money` (worker main.rs).
//!
//! **FlushToTreasurer:** если `state.treasurer_flush_to = Some(to)` — фоновый
//! `flush_treasurer` в main.rs уже долбит `/pay` до успеха. В `sell_pass`
//! **ничего** не делаем (flush сам справится). Просто **не** шлём `/balance`
//! из `sell_pass`, пока flush в полёте (не мешаем).
//!
//! **Таб-проверка — live.** `state.tab_names` залипает: `PlayerInfoRemove`
//! чистит только `seen_my_nicks` (по UUID), а `tab_names` (по именам) —
//! нет, потому что в пакете нет имён, только UUID. Поэтому для решений
//! «платить / не платить» используем `is_live_online(bot, nick)` —
//! `bot.player_uuid_by_username(nick).is_some()`, т.е. **актуальный**
//! таб на стороне клиента, а не кэш из `PlayerInfoUpdate`. Иначе
//! казначей слал `/pay` на myNick, которого уже нет в табе →
//! «Указанный игрок не найден».

use std::{sync::Arc, time::Duration};

use azalea::prelude::*;
use azalea_inventory::ItemStack;
use ipc::BotBoot;
use parking_lot::Mutex;
use serde_json::json;
use tracing::{info, warn};

use crate::{
    afk, cmd, delay,
    item_parse::{
        all_enchant_labels, durability_percent, get_slot_kind, match_slot_info, present, SlotKind,
    },
    listing,
    state::{BotState, SellAck},
    trash, warp,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellOutcome {
    Done,
    Partial,
    Retry,
}

const HOTBAR_START: usize = 36;
const HOTBAR_END: usize = 44;

/// KEEP_SUM — оставляем себе как farm-бот. Раздаём `bal - KEEP_SUM`.
const KEEP_SUM_BASE: f64 = 15_000_000.0;
fn keep_sum() -> f64 { ipc::balance::k(KEEP_SUM_BASE) }
/// Порог, при котором казначей **раздаёт** излишек (`bal - KEEP_SUM`).
const TREASURER_DISTRIBUTE_MIN: f64 = 500_000.0;
/// Минимальная сумма для `/pay` (мельче — не шлём, палево).
const MIN_PAY_AMOUNT: f64 = 200_000.0;
/// Порог «низкий баланс» — если у farm-бота меньше, докидываем до KEEP_SUM.
/// Нарочно ниже KEEP_SUM (13кк), чтобы казначей платил не часто.
const MIN_BOT_BALANCE_BASE: f64 = 13_000_000.0;
fn min_bot_balance() -> f64 { ipc::balance::k(MIN_BOT_BALANCE_BASE) }
/// Сентинел «баланс неизвестен».
const BALANCE_UNKNOWN: f64 = -1.0;
/// Минимальный остаток, чтобы отправить myNick. Меньше — копим на казначее
/// до следующего `sell_pass` (не палимся мелкими `/pay`).
const MY_NICK_MIN_PAY: f64 = 500_000.0;

pub async fn sell_pass(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) -> SellOutcome {
    {
        let mut s = state.lock();
        s.need_sell = false;   // ЕДИНСТВЕННОЕ место сброса
        s.storage_presence_sent = false;
    }
    if crate::safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }

    // своя позорная броня: надеваем до проверки каталога, drop_trash и продажи
    // (набор зашит в wear.rs, от Go не зависит; иначе позорный кусок могли бы выбросить или продать)
    crate::wear::equip_all(bot, state).await;

    let catalog_all = state.lock().catalog_all.clone();
    if catalog_all.is_empty() {
        warn!("sell → каталог пуст (ждём Go prices)");
        return SellOutcome::Retry;
    }

    if state.lock().afk {
        afk::un_afk(bot).await;
        state.lock().afk = false;
    }

    if trash::drop_trash(bot, boot, state).await {
        trash::do_rtp_if_needed(bot, state).await;
        return SellOutcome::Retry;
    }

    let mut promenade = {
        let mut s = state.lock();
        std::mem::take(&mut s.promenade)
    };
    warp::maybe_promenade(bot, &mut promenade).await;
    state.lock().promenade = promenade;

    // как JS moveToHotBar — забить хотбар до продажи
    fill_hotbar_from_backpack(bot, boot, state).await;

    let slots = player_inventory_slots(bot);
    let mut sold = 0u32;
    let mut full_stop = false;
    // за один проход выставляем не больше, чем слотов на АХ (5, а с арендой 10)
    let max_per_pass = crate::rent::slots(state) as u32;

    for idx in HOTBAR_START..=HOTBAR_END {
        if sold >= max_per_pass {
            break;
        }
        let Some(data) = slots.get(idx).and_then(present) else {
            continue;
        };
        let Some(info) = match_slot_info(bot, data, &catalog_all, &boot.go_type) else {
            continue;
        };

        let mut sell_price = state
            .lock()
            .need_sell_price
            .take()
            .unwrap_or(info.sell_price);
        if sell_price <= 0.0 {
            continue;
        }

        let enchants: Vec<_> = all_enchant_labels(bot, data)
            .into_iter()
            .map(|(n, l)| json!({ "name": n, "lvl": l }))
            .collect();
        let dur = durability_percent(data);

        delay::sell_gap().await;
        let quick = (idx - HOTBAR_START) as u8;
        bot.set_selected_hotbar_slot(quick);
        bot.wait_updates(2).await;
        let slot_gone = || sell_slot_is_empty(bot, boot, state, idx);
        if !delay::rnd_poll("SELL_HOTBAR", 50, slot_gone).await {
            info!("продажа → слот {idx} пуст до hotbar");
            continue;
        }

        let slots_now = player_inventory_slots(bot);
        let Some(data_now) = slots_now.get(idx).and_then(present) else {
            warn!("продажа → слот {idx} пуст после select");
            continue;
        };
        if match_slot_info(bot, data_now, &catalog_all, &boot.go_type).is_none() {
            warn!(
                "продажа → в руке не наш товар ({:?}), skip",
                data_now.kind
            );
            continue;
        }

        state.lock().sell_full = false;
        let mut stop_hotbar = false;
        for attempt in 1..=delay::SELL_SLOT_MAX_ATTEMPTS {
            if slot_gone() || state.lock().has_dangerous_trash {
                break;
            }
            if let Some(np) = state.lock().need_sell_price.take() {
                sell_price = np;
            }

            let alloc = {
                let cap = crate::rent::slots(state);
                let mut s = state.lock();
                listing::alloc_local(
                    &mut s.listings,
                    sell_price,
                    &info.id,
                    enchants.clone(),
                    Some(dur),
                    &boot.username,
                    cap,
                )
            };
            let Some((listing_id, list_price)) = alloc else {
                warn!("sell → нет свободного номера лота (слотов {})", crate::rent::slots(state));
                stop_hotbar = true;
                break;
            };

            let price_i = list_price.round() as i64;
            info!(
                "продажа → {} · {} (#{listing_id}) attempt {attempt}/{}",
                info.id,
                crate::money::money(list_price),
                delay::SELL_SLOT_MAX_ATTEMPTS
            );
            {
                let mut s = state.lock();
                s.held_sell_id = Some(info.id.clone());
                s.held_sell_price = Some(info.sell_price);
                s.held_nacenka = Some(info.nacenka);
                s.held_book_floor = Some(info.book_floor);
                s.held_book_mid = Some(info.book_mid);
                s.held_durability = Some(dur);
                s.sell_ack = SellAck::Waiting;
            }
            delay::rnd("SELL_CONFIRM").await;
            cmd::say(bot, format!("/ah sell {price_i}"));

            let mut ack = wait_sell_ack(
                state,
                Duration::from_millis(delay::SELL_LIST_ACK_TIMEOUT_MS),
            )
            .await;
            // old: timeout → retry если needPrice / ok если слот пуст / иначе confirm
            if ack == SellAck::Waiting || ack == SellAck::Idle {
                // ответ «Не удалось выставить» может прийти позже таймаута
                let mut waited = 0;
                while waited < 2000 && !state.lock().sell_full {
                    delay::rnd("POLL").await;
                    waited += 100;
                }
                if state.lock().sell_full {
                    ack = SellAck::Full;
                } else if state.lock().need_sell_price.is_some() {
                    ack = SellAck::Retry;
                } else if slot_gone() {
                    ack = SellAck::Ok;
                } else {
                    ack = SellAck::Confirm;
                }
            }

            if ack != SellAck::Ok && state.lock().sell_full {
                ack = SellAck::Full;
            }
            if ack != SellAck::Ok {
                state.lock().listings.clear_pending();
                listing::mirror_op(&boot.username, "clearPending", json!({}));
            }

            match ack {
                SellAck::Ok => {
                    // try-sell в Go шлёт chat-handler по «выставлен на продажу»
                    sold += 1;
                    break;
                }
                SellAck::Full => {
                    // как JS: стоп sell, витрину не сносим сразу — только по КД ~минута / timer60
                    warn!("продажа → АХ/хранилище забито");
                    stop_hotbar = true;
                    break;
                }
                SellAck::Empty | SellAck::Skip => {
                    tracing::debug!("продажа → skip/empty слот {idx}");
                    break;
                }
                SellAck::Retry | SellAck::Confirm => {
                    info!(
                        "продажа → повтор {attempt}/{} ({ack:?})",
                        delay::SELL_SLOT_MAX_ATTEMPTS
                    );
                    if attempt < delay::SELL_SLOT_MAX_ATTEMPTS && !slot_gone() {
                        delay::rnd("SELL_CONFIRM").await;
                        continue;
                    }
                    break;
                }
                SellAck::Waiting | SellAck::Idle => break,
            }
        }
        // Цена-«повтор» из ответа FunTime («Максимальная цена…») относится только к этому слоту.
        // Если попытки кончились на Retry, она оставалась в state и следующий предмет
        // выставлялся по ней (ботинки и sharp6 уходили на АХ по 10 млн).
        state.lock().need_sell_price = None;
        if stop_hotbar {
            full_stop = true;
            break;
        }
    }

    if sold > 0 {
        info!("продажа → выставлено {sold}");
    }
    // как JS finally: сброс needSell после прохода продажи
    // (УДАЛЕНО: state.lock().need_sell = false; — сброс только в начале)

    let more_left = (HOTBAR_START..=HOTBAR_END)
        .any(|i| !sell_slot_is_empty(bot, boot, state, i));
    let outcome = if full_stop {
        SellOutcome::Done
    } else if sold >= max_per_pass && more_left {
        SellOutcome::Partial
    } else {
        SellOutcome::Done
    };

    delay::rnd("BASE_DELAY").await;

    // ============================================================
    // КАЗНАЧЕЙ: flush / distribute
    // ============================================================

    // Если мне приказано отдать всё (FlushToTreasurer) — НЕ шлём /balance
    // здесь: фоновый `flush_treasurer` в main.rs уже долбит /balance + /pay
    // до успеха. Наш /balance только помешает (перетрёт `state.balance`).
    let flush_in_flight = {
        let st = state.lock();
        st.treasurer_flush_to.is_some() && st.treasurer_flush_in_flight
    };
    if flush_in_flight {
        info!("sell_pass → flush_to_treasurer в полёте, /balance пропущен");
        delay::rnd("BASE_DELAY").await;
        return SellOutcome::Done;
    }

    // После каждой прогулки-продажи — запросить баланс.
    // main.rs (parse_balance) увидит bal:
    //   - если я казначей и bal > KEEP_SUM + TREASURER_DISTRIBUTE_MIN →
    //     distribute_treasurer (раздать по state.balances_snapshot, потом myNick)
    //   - если я не казначей и bal > KEEP_SUM + EXCESS_THRESHOLD →
    //     /pay treasurer_nick
    delay::rnd("BASE_DELAY").await;
    if crate::request_balance(bot, state) {
        delay::rnd("BASE_DELAY").await; // ждём ответ на /balance
    }

    // Если я казначей — раздать излишек сразу после /balance.
    // `parse_balance` в main.rs тоже попробует, но там нет контекста
    // «я казначей» — он только сохранит `state.balance`. Раздача —
    // здесь, в sell_pass, где мы знаем, что продажа завершена.
    let (is_treasurer, snapshot, bal_opt) = {
        let st = state.lock();
        (
            st.is_treasurer,
            st.balances_snapshot.clone(),
            st.balance,
        )
    };
    if is_treasurer && state.lock().equalizing {
        tracing::debug!("sell_pass → идёт выравнивание, казначей сам не раздаёт");
    } else if is_treasurer {
        if let Some(bal) = bal_opt {
            let to_send = (bal - keep_sum()).floor();
            if to_send >= TREASURER_DISTRIBUTE_MIN {
                info!(
                    "sell_pass → казначей distribute: bal={} to_send={} (снимок {} ботов)",
                    crate::money::money(bal),
                    crate::money::money(to_send),
                    snapshot.len(),
                );
                distribute_treasurer(bot, boot, state, bal, &snapshot).await;
            } else {
                tracing::debug!(
                    "sell_pass → казначей, излишек {} < {} — не раздаём (и myNick не шлём)",
                    crate::money::money(to_send),
                    crate::money::money(TREASURER_DISTRIBUTE_MIN),
                );
            }
        }
    }

    // ============================================================
    // АРЕНДА СЛОТОВ АХ (rent.rs) — здесь же, где пишется баланс и делаются переводы: баланс только что
    // обновлён, деньги разданы. Раз в час на бота, в выгодные часы, если хватает денег.
    // ============================================================
    if crate::rent::due(state) {
        info!("sell_pass → аренда слотов (после /balance и переводов)");
        let _ = crate::rent::run_rent(bot, state).await;
    }

    outcome
}

/// Хвост `sell_pass` для режима «инвиз» (invis.rs): казначей раздаёт излишек по снимку балансов.
/// `/balance` вызывающий уже отправил; берём `state.balance`.
pub async fn treasurer_pass(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    let (is_treasurer, snapshot, bal_opt, equalizing, flush) = {
        let st = state.lock();
        (
            st.is_treasurer,
            st.balances_snapshot.clone(),
            st.balance,
            st.equalizing,
            st.treasurer_flush_to.is_some() && st.treasurer_flush_in_flight,
        )
    };
    if !is_treasurer || equalizing || flush {
        return;
    }
    if let Some(bal) = bal_opt {
        let to_send = (bal - keep_sum()).floor();
        if to_send >= TREASURER_DISTRIBUTE_MIN {
            distribute_treasurer(bot, boot, state, bal, &snapshot).await;
        }
    }
}

/// Казначей раздаёт излишек (`bal - KEEP_SUM`) другим farm-ботам.
/// Логика как `owner::redistribute`, но:
///   - `available = bal - KEEP_SUM` (оставляем себе как farm-бот)
///   - таб-проверка **live** через `bot.player_uuid_by_username` перед каждым `/pay`
///   - `/pay` **×2** (FunTime требует подтверждения — см. `pay_once`)
///   - приоритет: сначала тем, у кого `bal < MIN_BOT_BALANCE`, докидываем
///     до `KEEP_SUM`; остаток — по `per_bot` (выравнивание).
///   - **`actually_sent`** — точный учёт отправленного. `remaining`
///     считается как `available - actually_sent`, а не по `state.balance`
///     (который мог не обновиться, если `/pay` не прошёл).
///   - **после раздачи** — остаток `remaining` уходит на `my_nick`
///     (если он в табе и `remaining > MY_NICK_MIN_PAY`).
async fn distribute_treasurer(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    owner_bal: f64,
    snapshot: &[(String, f64)],
) -> u32 {
    if state.lock().banned || state.lock().shutdown {
        return 0;
    }
    if !state.lock().on_anarchy {
        info!("казначей distribute → не на анке, пропуск");
        return 0;
    }

    // Early-exit: если **live** таб пуст — никого нет, раздавать некому
    // и myNick тоже нет. `is_live_online` для каждого ника отдельно
    // делать не будем — просто проверим, что таб не пуст.
    if bot.tab_list().is_empty() {
        info!("казначей distribute → таб пуст (live), пропуск");
        return 0;
    }

    let available = (owner_bal - keep_sum()).floor();
    if available < MIN_PAY_AMOUNT {
        tracing::debug!("казначей distribute → available < MIN_PAY_AMOUNT");
        return 0;
    }

    // needs: кто < MIN_BOT_BALANCE — им shortfall до KEEP_SUM.
    // Пустой snapshot — норм, значит ботов нет (или все богатые) —
    // тогда весь available пойдёт на myNick.
    let mut needs: Vec<(String, f64, bool)> = snapshot
        .iter()
        .filter(|(nick, _)| {
            // не платим себе
            !nick.eq_ignore_ascii_case(&boot.username)
        })
        .filter_map(|(nick, bal)| {
            if (*bal - BALANCE_UNKNOWN).abs() < 0.5 {
                // неизвестно — резервируем как KEEP_SUM (не платим)
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

    // Фильтр по **live** табу: не платим тем, кого нет в табе.
    let mut missing_in_tab = 0u32;
    let mut payable: Vec<(String, f64)> = Vec::new();
    let mut total_needed: f64 = 0.0;
    for (nick, shortfall, unknown) in &needs {
        if *unknown {
            total_needed += shortfall;
            continue;
        }
        if !is_live_online(bot, nick) {
            missing_in_tab += 1;
            continue;
        }
        total_needed += shortfall;
        payable.push((nick.clone(), *shortfall));
    }
    if missing_in_tab > 0 {
        info!(
            "казначей distribute → {} ник(ов) не в табе (live), пропуск /pay",
            missing_in_tab
        );
    }

    let mut paid_count = 0u32;
    let mut actually_sent: f64 = 0.0;
    let mut remaining = available;

    if payable.is_empty() {
        info!(
            "казначей distribute → платить ботам некому (available={}, total_needed={})",
            crate::money::money(available),
            crate::money::money(total_needed),
        );
    } else if remaining >= total_needed {
        // Хватает всем — добиваем до KEEP_SUM.
        for (nick, shortfall) in &payable {
            let amount = shortfall.floor() as i64;
            if amount < MIN_PAY_AMOUNT as i64 {
                continue;
            }
            if !is_live_online(bot, nick) {
                info!("казначей → {nick} вышел из таба (live), пропуск");
                continue;
            }
            info!(
                "казначей → {nick}: /pay {} (добить до KEEP_SUM, shortfall={})",
                crate::money::money(amount as f64),
                crate::money::money(*shortfall),
            );
            pay_once(bot, state, nick, amount).await;
            remaining -= amount as f64;
            actually_sent += amount as f64;
            paid_count += 1;
        }
        info!(
            "казначей distribute → раздал ботам: {paid_count} /pay, actually_sent={}, remaining={}",
            crate::money::money(actually_sent),
            crate::money::money(remaining),
        );
    } else {
        // Не хватает — выравнивание по per_bot.
        let mut known: Vec<(String, f64)> = payable;
        while remaining >= MIN_PAY_AMOUNT && !known.is_empty() {
            let n = known.len() as f64;
            let per_bot = (remaining / n).floor();

            if per_bot < MIN_PAY_AMOUNT {
                let (nick, shortfall) = known.remove(0);
                let amount_f = shortfall.min(remaining).floor();
                if amount_f < MIN_PAY_AMOUNT {
                    info!(
                        "казначей → {nick} shortfall={}, remaining={} — платить нечего",
                        crate::money::money(shortfall),
                        crate::money::money(remaining),
                    );
                    break;
                }
                if !is_live_online(bot, &nick) {
                    info!("казначей → {nick} вышел из таба (live), пропуск");
                    break;
                }
                let amount = amount_f as i64;
                info!(
                    "казначей → {nick}: /pay {} [последний]",
                    crate::money::money(amount as f64),
                );
                pay_once(bot, state, &nick, amount).await;
                remaining -= amount_f;
                actually_sent += amount_f;
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
                if !is_live_online(bot, &nick) {
                    info!("казначей → {nick} вышел из таба (live), пропуск");
                    known.remove(idx);
                    continue;
                }
                let amount = amount_f as i64;
                info!(
                    "казначей → {nick}: /pay {} (per_bot={})",
                    crate::money::money(amount as f64),
                    crate::money::money(per_bot),
                );
                pay_once(bot, state, &nick, amount).await;
                remaining -= amount_f;
                actually_sent += amount_f;
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
                info!("казначей distribute → никто не получил, стоп");
                break;
            }
        }
        info!(
            "казначей distribute → выравнивание: {paid_count} /pay, actually_sent={}, remaining={}",
            crate::money::money(actually_sent),
            crate::money::money(remaining),
        );
    }

    // ============================================================
    // ПОСЛЕ раздачи — отправить остаток myNick (если в табе).
    // remaining = available - actually_sent (точный, не по state.balance).
    // ============================================================
    pay_my_nick_remaining(bot, boot, state, remaining).await;

    paid_count
}

/// Отправить остаток (`remaining`) на `my_nick`, если он в табе и
/// remaining > MY_NICK_MIN_PAY. **`/pay` ×2** (FunTime требует
/// подтверждения — иначе перевод не проходит, см. `pay_once`).
///
/// Таб-проверка — **live** через `is_live_online` (`bot.player_uuid_by_username`),
/// не через `state.tab_names`. Иначе `tab_names` залипает (выход с анки
/// не чистит по именам) и `/pay` уходит на отсутствующего → «Указанный
/// игрок не найден».
///
/// Защита: если `my_nick == boot.username` (казначей = твой ник) — пропуск
/// (FunTime может не пропустить `/pay` самому себе).
async fn pay_my_nick_remaining(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    remaining: f64,
) {
    let Some(my_nick) = boot.my_nick.clone() else {
        tracing::debug!("pay_my_nick_remaining → my_nick не задан");
        return;
    };
    if my_nick.is_empty() {
        return;
    }
    if my_nick.eq_ignore_ascii_case(&boot.username) {
        info!(
            "pay_my_nick_remaining → my_nick {my_nick} = я сам (казначей), пропуск"
        );
        return;
    }
    if remaining < MY_NICK_MIN_PAY {
        tracing::debug!(
            "pay_my_nick_remaining → remaining={} < MY_NICK_MIN_PAY={}, пропуск (копим)",
            crate::money::money(remaining),
            crate::money::money(MY_NICK_MIN_PAY),
        );
        return;
    }
    if state.lock().banned || state.lock().shutdown {
        return;
    }
    if !is_live_online(bot, &my_nick) {
        info!(
            "pay_my_nick_remaining → myNick {my_nick} не в табе (live), пропуск (не палимся)"
        );
        return;
    }
    let amount = remaining.floor() as i64;
    info!(
        "казначей → myNick {my_nick}: /pay {} (остаток после раздачи ботам)",
        crate::money::money(amount as f64),
    );
    afk::un_afk(bot).await;
    state.lock().afk = false;
    // **×2:** первая — запрос, вторая — подтверждение (FunTime).
    // Без второй сервер отвечает «Необходимо подтверждение» и
    // перевод не проходит. Это та же логика, что в `pay_once`.
    delay::rnd("BASE_DELAY").await;
    if !cmd::pay(bot, &my_nick, amount) {
        return;
    }
    let ms = 500 + (rand::random::<u64>() % 501);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    if !cmd::pay(bot, &my_nick, amount) {
        return;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

/// `/pay` **дважды** — FunTime требует подтверждения. Первая команда
/// — запрос, вторая — подтверждение. Без второй перевод не проходит
/// (сервер отвечает «Необходимо подтверждение»). Это та же логика,
/// что в `flush_treasurer_loop` и `excess_money` (worker main.rs).
///
/// **Все** `/pay` в проекте шлются **×2** через эту функцию (или
/// аналогичную в worker main.rs). Никогда не шли одну `/pay` вручную.
pub(crate) async fn pay_once(bot: &Client, state: &Arc<Mutex<BotState>>, to: &str, amount: i64) {
    afk::un_afk(bot).await;
    let mut st = state.lock();
    st.afk = false;
    st.last_my_nick_pay = Some(std::time::Instant::now());
    drop(st);
    // Первая — запрос.
    delay::rnd("BASE_DELAY").await;
    if !cmd::pay(bot, to, amount) {
        return;
    }
    let ms = 500 + (rand::random::<u64>() % 501);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    // Вторая — подтверждение (FunTime).
    if !cmd::pay(bot, to, amount) {
        return;
    }
    // Пауза, чтобы сервер обработал обе команды.
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

async fn wait_sell_ack(state: &Arc<Mutex<BotState>>, timeout: Duration) -> SellAck {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        let ack = state.lock().sell_ack;
        if ack != SellAck::Waiting && ack != SellAck::Idle {
            state.lock().sell_ack = SellAck::Idle;
            return ack;
        }
        delay::rnd("POLL").await;
    }
    state.lock().sell_ack = SellAck::Idle;
    SellAck::Waiting
}

/// Заполняет пустые/мусорные слоты хотбара каталоговым товаром (JS moveToHotBar).
///
/// Как живой игрок: всё перекладывание — одно открытие инвентаря (E → клики → Esc), один `ContainerClose 0`
/// в конце. Раньше инвентарь «открывался» и закрывался на каждый предмет, а слот в руке переключался внутри —
/// при открытом инвентаре игрок так сделать не может (слот перед `/ah sell` выбирает `sell_pass`).
async fn fill_hotbar_from_backpack(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    if state.lock().has_dangerous_trash {
        return;
    }
    let _ = trash::drop_trash(bot, boot, state).await;

    let mut handle = None;
    for _ in 0..9 {
        let Some(mv) = plan_backpack_move(bot, boot, state) else {
            break;
        };
        if handle.is_none() {
            if crate::safe_inv::try_close(bot) {
                delay::rnd("POLL").await;
            }
            let Some(h) = bot.open_inventory() else {
                break;
            };
            delay::rnd("HOTBAR_DELAY").await;
            handle = Some(h);
        } else {
            delay::rnd("HOTBAR_DELAY").await; // пауза между предметами
        }
        let Some(h) = handle.as_ref() else { break };
        if mv.drop_hot_first {
            // в целевом слоте хотбара мусор — выбросить его за окно
            h.left_click(mv.hot);
            delay::rnd("SELL_HOTBAR").await;
            h.click(azalea_inventory::operations::PickupClick::LeftOutside);
            delay::rnd("HOTBAR_DELAY").await;
        }
        h.left_click(mv.src);
        delay::rnd("SELL_HOTBAR").await;
        h.left_click(mv.hot);
        info!("продажа → перенёс в хотбар");
        delay::rnd("POLL").await;
    }
    if let Some(h) = handle {
        // от последнего клика до закрытия у живого игрока ≈0,5 с (записи бот-вью)
        tokio::time::sleep(std::time::Duration::from_millis(350 + rand::random::<u64>() % 500)).await;
        drop(h); // ContainerClose 0 — один раз на всю раскладку
    }
}

struct BackpackMove {
    /// куда в хотбаре (36–44)
    hot: usize,
    /// откуда из рюкзака (9–35)
    src: usize,
    /// в `hot` лежит мусор — сначала выбросить
    drop_hot_first: bool,
}

/// Следующий перенос «рюкзак → хотбар» по текущему инвентарю: пустой или мусорный слот хотбара и каталоговый
/// товар в рюкзаке. `None` — переносить нечего.
fn plan_backpack_move(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) -> Option<BackpackMove> {
    let catalog_all = state.lock().catalog_all.clone();
    let slots = player_inventory_slots(bot);
    let mut target: Option<(usize, bool)> = None;
    for i in HOTBAR_START..=HOTBAR_END {
        let Some(data) = slots.get(i).and_then(present) else {
            target = Some((i, false));
            break;
        };
        match get_slot_kind(bot, data, &catalog_all, &boot.go_type) {
            SlotKind::Trash => {
                target = Some((i, true));
                break;
            }
            SlotKind::Trade(_) | SlotKind::Foreign { .. } => continue,
        }
    }
    let (hot, drop_hot_first) = target?;
    let src = (9..36).find(|&idx| {
        slots
            .get(idx)
            .and_then(present)
            .is_some_and(|data| match_slot_info(bot, data, &catalog_all, &boot.go_type).is_some())
    })?;
    Some(BackpackMove { hot, src, drop_hot_first })
}

fn sell_slot_is_empty(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    idx: usize,
) -> bool {
    let slots = player_inventory_slots(bot);
    let Some(data) = slots.get(idx).and_then(present) else {
        return true;
    };
    let catalog_all = state.lock().catalog_all.clone();
    match get_slot_kind(bot, data, &catalog_all, &boot.go_type) {
        SlotKind::Trade(_) => false,
        SlotKind::Trash | SlotKind::Foreign { .. } => true,
    }
}

pub fn count_our_items(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) -> u32 {
    inventory_catalog_ids(bot, boot, state).len() as u32
}

pub fn inventory_catalog_ids(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> Vec<String> {
    let catalog_all = state.lock().catalog_all.clone();
    let slots = player_inventory_slots(bot);
    let mut ids = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        if i < 9 || i > 40 {
            continue;
        }
        let Some(data) = present(slot) else { continue };
        if let Some(info) = match_slot_info(bot, data, &catalog_all, &boot.go_type) {
            ids.push(info.id);
        }
    }
    ids
}

pub fn inventory_full(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) -> bool {
    count_our_items(bot, boot, state) >= 27
}

fn player_inventory_slots(bot: &Client) -> Vec<ItemStack> {
    crate::safe_inv::try_player_slots(bot).unwrap_or_default()
}

// ============================================================
// КАЗНАЧЕЙ: helpers
// ============================================================

/// Снимок таба: онлайн-ники (lowercase). Аналогично owner.rs.
/// **Оставлено** для обратной совместимости / логов, но для решений
/// «платить / не платить» используй `is_live_online(bot, nick)` —
/// `state.tab_names` залипает (выход с анки не чистит по именам).
#[allow(dead_code)]
fn tab_snapshot(state: &Arc<Mutex<BotState>>) -> std::collections::HashSet<String> {
    state
        .lock()
        .tab_names
        .iter()
        .map(|n| n.to_lowercase())
        .collect()
}

/// Ник **сейчас** в табе? Спрашиваем у `Client` (`player_uuid_by_username`),
/// а не у `state.tab_names`. Это чинит залипание: `PlayerInfoRemove`
/// чистит только `seen_my_nicks` (по UUID), а `tab_names` (по именам) —
/// нет, потому что в пакете нет имён. Из-за этого казначей слал `/pay`
/// на myNick, которого уже нет в табе → «Указанный игрок не найден».
///
/// Если `player_uuid_by_username` в твоей версии Azalea называется иначе
/// (например, `tab_list()` + перебор по `profile.name`) — замени тело
/// на `bot.tab_list().values().any(|p| p.profile.name.eq_ignore_ascii_case(nick))`.
fn is_live_online(bot: &Client, nick: &str) -> bool {
    bot.player_uuid_by_username(nick).is_some()
}