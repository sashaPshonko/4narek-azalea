//! Хранилище АХ: sync listing, снятие, reload, назад в АХ.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use serde_json::json;
use tracing::{info, warn};

use crate::{
    delay,
    item_parse::{ah_price, get_slot_kind, match_slot_info, present, SlotKind},
    listing::{self, ListingMemory},
    sell,
    state::BotState,
};

/// Сколько первых слотов окна хранилища разбираем: 5 без аренды, 10 с арендой. Берём максимум всегда:
/// пустые слоты пропускаются, а лоты арендованных слотов после конца аренды ещё могут лежать в окне.
const STORAGE_AH_SLOTS: usize = 10;
const SLOT_BACK_AH: usize = 46;
const SLOT_RELOAD: usize = 50;
const SLOT_RESET: usize = 52;
/// Сколько ждать в окне подтверждения сброса витрины после клика.
const RESET_CONFIRM_WAIT: Duration = Duration::from_secs(5);
/// Сколько ждать, пока после «назад в АХ» сервер откроет другое окно (сменится container id).
const NEW_WINDOW_TIMEOUT: Duration = Duration::from_secs(3);

/// Сменилось ли окно относительно `old_id` (по container id из OpenScreen).
fn window_changed(old_id: i32, now_id: i32) -> bool {
    now_id != old_id
}

/// Ждёт, пока сервер откроет другое окно. Как в JS: следующий шаг только после нового `windowOpen`, а не по
/// старому заголовку (иначе бот после «назад» снова заходит в хранилище по устаревшему `last_window_title`).
pub async fn wait_new_window(state: &Arc<Mutex<BotState>>, old_id: i32, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if window_changed(old_id, state.lock().last_window_id) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Ждёт момента клика по кнопке сброса (60 с + запас от последнего сброса). `false` — окно сменилось/закрылось.
async fn wait_reset_moment(bot: &Client, state: &Arc<Mutex<BotState>>, win_id: i32) -> bool {
    // Случайная добавка выбирается один раз на сброс: клик идёт через 60,5–62,8 с после прошлого, а не ровно в 60.
    let jitter = Duration::from_millis(rand::Rng::gen_range(
        &mut rand::thread_rng(),
        crate::state::RESET_JITTER_MS,
    ));
    let first = crate::state::reset_click_wait(state.lock().last_storage_reset.elapsed(), jitter);
    if first.is_zero() {
        delay::rnd("GLASS").await;
    }
    loop {
        let left = crate::state::reset_click_wait(state.lock().last_storage_reset.elapsed(), jitter);
        if left.is_zero() {
            return crate::safe_inv::try_id(bot) == Some(win_id);
        }
        if state.lock().shutdown || crate::safe_inv::try_id(bot) != Some(win_id) {
            return false;
        }
        tokio::time::sleep(left.min(Duration::from_millis(100))).await;
    }
}

pub async fn handle_storage(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> StorageResult {
    let res = handle_storage_inner(bot, boot, state).await;
    // Выходим из хранилища (закрыто, ушли в АХ, продали) — окно больше не считается хранилищем.
    if !matches!(res, StorageResult::Stay) {
        state.lock().storage_win_id = None;
    }
    res
}

async fn handle_storage_inner(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> StorageResult {
    // Не человеческая пауза, а техническая: даём 150 мс на приход содержимого окна после открытия.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // id окна, на основании которого принимаем решения: все клики ниже проверяют, что оно всё ещё открыто.
    let win_id = match crate::safe_inv::try_id(bot) {
        Some(id) if id != 0 => id,
        _ => return StorageResult::Closed,
    };

    // Память лотов перестраиваем по тому, что реально лежит в слотах 0–4 (записи из памяти не доверяем).
    state.lock().storage_win_id = Some(win_id);
    let rows = rebuild_listing_memory(bot, boot, state);
    tracing::debug!("хранилище → память лотов перестроена по слотам: {rows} лот(ов)");

    // Если нужен reload АХ, делаем его и остаёмся в storage.
    if state.lock().need_reload_ah {
        {
            let mut s = state.lock();

            s.need_reload_ah = false;
            s.storage_stale_unlist_streak = 0;
            s.storage_stale_unlist_slot = -1;
            s.storage_stale_unlist_key.clear();
        }

        reload_storage(bot, win_id).await;
        return StorageResult::Stay;
    }

    // Зашли в хранилище → needReset снят.
    state.lock().need_reset = false;

    // Presence: предметы на витрине + предметы в инвентаре.
    if !state.lock().storage_presence_sent {
        report_storage_items(bot, boot, state);
        state.lock().storage_presence_sent = true;
    }

    // Сброс витрины раз в 60 секунд через слот 52,
    // если слот 0 занят.
    //
    // Это НЕ КД-снятие. Таймер сброса НЕ трогаем здесь: он обновляется по сообщению
    // «[✔] Предметы успешно перевыставлены!» (main.rs), иначе неудавшийся клик терял бы целую минуту.
    let need_reset = state.lock().reset_soon_now();

    if need_reset {
        let has0 = crate::safe_inv::try_contents(bot)
            .map(|c| c.get(0).and_then(present).is_some())
            .unwrap_or(false);

        if has0 {
            info!("хранилище → сброс витрины");

            // Входили заранее: ждём ровно до 60 с от прошлого сброса (+ запас), а не случайную паузу 1,5–4,5 с.
            // Если срок уже прошёл — короткая «человеческая» пауза.
            if !wait_reset_moment(bot, state, win_id).await {
                return StorageResult::Closed;
            }
            if !crate::safe_inv::try_left_click_expect(bot, SLOT_RESET, win_id) {
                return StorageResult::Closed;
            }
            state.lock().last_reset_click_at = Some(Instant::now());

            // Ждём подтверждение прямо в окне (сообщение чата обнуляет last_reset_click_at): уходить раньше нельзя,
            // иначе бот сразу уходит в продажу, сброс срывается и таймер съезжает.
            let wait_from = Instant::now();
            while wait_from.elapsed() < RESET_CONFIRM_WAIT {
                if state.lock().last_reset_click_at.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if state.lock().last_reset_click_at.is_some() {
                warn!("хранилище → сброс витрины: подтверждения нет за {} с", RESET_CONFIRM_WAIT.as_secs());
                dump_storage_window(bot, boot, state, "reset_no_confirm");
            }
            return StorageResult::Stay;
        }

        // На витрине нечего перевыставлять — сообщения не будет, считаем сброс выполненным.
        state.lock().last_storage_reset = Instant::now();
    }

    // ============================================================
    // ОЧИСТКА ВИТРИНЫ ПЕРЕД ПРОДАЖЕЙ
    //
    // Если инвентарь заполнен, снимаем предметы ПО ОДНОМУ.
    //
    // После каждого клика ждём обновления GUI и возвращаемся
    // в цикл storage.
    //
    // Следующий вызов handle_storage() снимет следующий слот.
    //
    // Продажа происходит ТОЛЬКО когда больше нечего снимать.
    // ============================================================

    let inventory_full = sell::inventory_full(bot, boot, state);

    if inventory_full {
        // Помечаем, что после полной очистки витрины нужна продажа.
        state.lock().need_sell = true;
    }

    let unlist = find_slot_to_unlist(bot, boot, state);

    if let Some((slot, reason)) = unlist {
        // Снятие — триггер продажи, без проверки предметов.
        state.lock().need_sell = true;

        let storage_key = storage_content_key(bot);

        // Тот же слот + тот же GUI >= 2 раза → reload 50.
        // Это защита от зависшего/необновившегося GUI.
        {
            let mut s = state.lock();

            if slot as i32 == s.storage_stale_unlist_slot
                && storage_key == s.storage_stale_unlist_key
                && s.storage_stale_unlist_streak >= 2
            {
                info!(
                    "хранилище → залип слот {slot} ×{} → reload",
                    s.storage_stale_unlist_streak
                );

                s.storage_stale_unlist_streak = 0;
                s.storage_stale_unlist_slot = -1;
                s.storage_stale_unlist_key.clear();

                drop(s);

                reload_storage(bot, win_id).await;
                return StorageResult::Stay;
            }
        }

        info!("хранилище → снимаю #{slot} ({reason})");

        let content_before = storage_key;

        delay::rnd("STORAGE_UNLIST").await;
        if !crate::safe_inv::try_left_click_expect(bot, slot, win_id) {
            return StorageResult::Closed;
        }

        // Ждём обновления GUI после снятия.
        let settle =
            wait_storage_gui_settle(bot, &content_before).await;

        let content_after = storage_content_key(bot);

        {
            let mut s = state.lock();

            match settle {
                GuiSettleResult::Changed => {
                    s.storage_stale_unlist_streak = 0;
                    s.storage_stale_unlist_slot = -1;
                    s.storage_stale_unlist_key.clear();
                }

                GuiSettleResult::NoWindow => {
                    // GUI закрылось — считаем это изменением.
                    s.storage_stale_unlist_streak = 0;
                    s.storage_stale_unlist_slot = -1;
                    s.storage_stale_unlist_key.clear();
                }

                GuiSettleResult::Timeout => {
                    // GUI не сменился — считаем застой.
                    if content_after == content_before
                        && slot as i32 == s.storage_stale_unlist_slot
                        && content_before == s.storage_stale_unlist_key
                    {
                        s.storage_stale_unlist_streak += 1;
                    } else if content_after == content_before {
                        s.storage_stale_unlist_streak = 1;
                        s.storage_stale_unlist_slot = slot as i32;
                        s.storage_stale_unlist_key = content_before;
                    } else {
                        s.storage_stale_unlist_streak = 0;
                        s.storage_stale_unlist_slot = -1;
                        s.storage_stale_unlist_key.clear();
                    }

                    if s.storage_stale_unlist_streak >= 2 {
                        s.need_reload_ah = true;
                    }
                }
            }
        }

        // КРИТИЧЕСКИ ВАЖНО:
        //
        // Здесь НЕТ sell_pass().
        //
        // Следующий цикл снова зайдёт в storage
        // и снимет следующий слот.
        return StorageResult::Unlisted;
    }

    // Сюда попадаем только когда больше нечего снимать.
    {
        let mut s = state.lock();

        s.storage_stale_unlist_streak = 0;
        s.storage_stale_unlist_slot = -1;
        s.storage_stale_unlist_key.clear();
    }

    // ============================================================
    // ПРОДАЖА
    //
    // Если раньше был полный инвентарь и мы уже сняли всю витрину,
    // только теперь идём продавать.
    //
    // need_sell также может быть установлен другими механизмами
    // бота, поэтому обычная продажа по need_sell сохраняется.
    // ============================================================

    let should_sell = state.lock().need_sell;
    // До следующего сброса витрины меньше 15 с — продажу не начинаем (сдвинула бы ритм), она пойдёт после сброса.
    let sale_blocked = state.lock().sale_blocked_by_reset_now();
    if should_sell && sale_blocked {
        info!("хранилище → до сброса витрины меньше 15с, продажу отложил");
    }

    if should_sell && !sale_blocked {
        info!("хранилище → витрина очищена, иду продавать");

        let _ = crate::safe_inv::try_close(bot);

        delay::rnd("500MS").await;

        let outcome = sell::sell_pass(bot, boot, state).await;
        if matches!(outcome, sell::SellOutcome::Partial | sell::SellOutcome::Retry) {
            state.lock().need_sell = true;
        }

        return StorageResult::Sold;
    }

    // Назад в АХ.
    info!("хранилище → назад в АХ");

    delay::rnd("STORAGE_BACK").await;
    if crate::safe_inv::try_left_click_expect(bot, SLOT_BACK_AH, win_id) {
        delay::rnd("POLL").await;
        // Ждём новое окно (АХ). Не дождались — «назад» не сработал: закрываем и даём циклу открыть АХ заново.
        if !wait_new_window(state, win_id, NEW_WINDOW_TIMEOUT).await {
            warn!("хранилище → назад в АХ: окно не сменилось за {}с, закрываю", NEW_WINDOW_TIMEOUT.as_secs());
            let _ = crate::safe_inv::try_close(bot);
            return StorageResult::Closed;
        }
    } else {
        // окно уже другое (или закрылось) — выходим, цикл разберётся по актуальному окну
        return StorageResult::Closed;
    }

    StorageResult::BackToAh
}

pub async fn reload_storage(bot: &Client, win_id: i32) {
    let content_before = storage_content_key(bot);

    delay::rnd("WINDOW_DELAY").await;
    if !crate::safe_inv::try_left_click_expect(bot, SLOT_RELOAD, win_id) {
        return;
    }

    info!("хранилище → обновляю");

    // Ждём обновления GUI (пауза уже была ПЕРЕД кликом).
    let _ = wait_storage_gui_settle(bot, &content_before).await;
}

fn storage_content_key(bot: &Client) -> String {
    let Some(contents) = crate::safe_inv::try_contents(bot) else {
        return String::new();
    };

    let mut parts = Vec::new();

    for (i, slot) in contents
        .iter()
        .enumerate()
        .take(STORAGE_AH_SLOTS)
    {
        if let Some(d) = present(slot) {
            let p = ah_price(d).unwrap_or(0.0) as i64;

            parts.push(format!("{i}:{:?}:{p}", d.kind));
        } else {
            parts.push(format!("{i}:_"));
        }
    }

    parts.join("|")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageResult {
    Closed,
    Stay,
    Unlisted,
    Sold,
    BackToAh,
}

fn storage_prices(bot: &Client) -> Vec<f64> {
    let Some(contents) = crate::safe_inv::try_contents(bot) else {
        return Vec::new();
    };

    let mut prices = Vec::new();

    for i in 0..STORAGE_AH_SLOTS {
        if let Some(data) = contents.get(i).and_then(present) {
            if let Some(p) = ah_price(data) {
                if p > 0.0 {
                    prices.push(p);
                }
            }
        }
    }

    prices
}

/// Лоты из слотов 0–4 хранилища: (записи памяти по торгуемым лотам, цены всех лотов окна).
fn read_storage_rows(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> Option<(Vec<listing::ListingMeta>, Vec<f64>)> {
    let contents = crate::safe_inv::try_contents(bot)?;
    let catalog_all = state.lock().catalog_all.clone();

    let mut rows = Vec::new();
    let mut prices = Vec::new();

    for i in 0..STORAGE_AH_SLOTS {
        let Some(data) = contents.get(i).and_then(present) else {
            continue;
        };
        let Some(price) = ah_price(data) else {
            continue;
        };
        if price <= 0.0 {
            continue;
        }
        prices.push(price);

        if let SlotKind::Trade(info) =
            get_slot_kind(bot, data, &catalog_all, &boot.go_type)
        {
            rows.push(listing::ListingMeta {
                listing_id: (price as i64 % 10) as u8,
                catalog_id: info.id,
                enchants: crate::item_parse::all_enchant_labels(bot, data)
                    .into_iter()
                    .map(|(n, l)| json!({ "name": n, "lvl": l }))
                    .collect(),
                durability: Some(crate::item_parse::durability_percent(data)),
                price,
            });
        }
    }

    Some((rows, prices))
}

/// Перестроить память лотов по содержимому открытого хранилища. Возвращает число торгуемых лотов.
pub fn rebuild_listing_memory(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> usize {
    let Some((rows, prices)) = read_storage_rows(bot, boot, state) else {
        return 0;
    };
    let n = rows.len();

    state.lock().listings.rebuild_from_storage(rows, &prices);

    listing::mirror_op(&boot.username, "sync", json!({ "prices": prices }));
    n
}

/// Перед отправкой try-sell: если бот всё ещё в том самом окне хранилища, обновить память по слотам.
pub fn refresh_listing_memory_if_in_storage(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> bool {
    let want = state.lock().storage_win_id;
    match (want, crate::safe_inv::try_id(bot)) {
        (Some(w), Some(now)) if w == now && now != 0 => {
            rebuild_listing_memory(bot, boot, state);
            true
        }
        _ => false,
    }
}

fn find_slot_to_unlist(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> Option<(usize, String)> {
    let contents = crate::safe_inv::try_contents(bot)?;

    let inv_full = sell::inventory_full(bot, boot, state);

    let catalog_all = state.lock().catalog_all.clone();

    let mut price_or_full: Option<(usize, String)> = None;

    // Идём с 4 к 0, как в старой логике.
    for i in (0..STORAGE_AH_SLOTS).rev() {
        let Some(data) = contents.get(i).and_then(present) else {
            continue;
        };

        // Foreign → пропускаем.
        // Trash → снимаем.
        // Trade → проверяем цену / снимаем при полном инвентаре.
        let info = match get_slot_kind(
            bot,
            data,
            &catalog_all,
            &boot.go_type,
        ) {
            SlotKind::Foreign { .. } => continue,

            SlotKind::Trash => {
                // Что именно считаем мусором: предмет, чары, прочность, цена. Мусор без SKU в хранилище бывает
                // у проданных лотов и редких случаев (прочность < 50% без починки, нет цены в каталоге Go).
                let enchants: Vec<String> = crate::item_parse::all_enchant_labels(bot, data)
                    .into_iter()
                    .map(|(n, l)| format!("{}{}", n.trim_start_matches("minecraft:"), l))
                    .collect();
                warn!(
                    "хранилище → слот {i}: мусор: {} · прочность {:.0}% · цена {:?} · чары [{}]",
                    crate::item_parse::item_minecraft_name(data),
                    crate::item_parse::durability_percent(data) * 100.0,
                    ah_price(data),
                    enchants.join(" ")
                );
                return Some((
                    i,
                    "мусор (нет в каталоге)".into(),
                ));
            }

            SlotKind::Trade(info) => info,
        };

        let Some(price) = ah_price(data) else {
            return Some((
                i,
                "мусор (не читается цена)".into(),
            ));
        };

        // Ожидаемая цена — по самому лоту в слоте хранилища (каталог + прочность + починка), не по памяти бота.
        if !ListingMemory::prices_aligned(price, info.sell_price)
            && price_or_full.is_none()
        {
            price_or_full = Some((
                i,
                format!(
                    "цена {price} ≠ {} (прочность {:.0}%)",
                    info.sell_price,
                    crate::item_parse::durability_percent(data) * 100.0
                ),
            ));

            continue;
        }

        // При полном инвентаре снимаем предметы с витрины.
        //
        // Важно: здесь выбирается только ОДИН слот.
        // Следующий будет выбран следующим вызовом
        // handle_storage().
        if inv_full && price_or_full.is_none() {
            price_or_full = Some((
                i,
                "инвентарь ≥27 — снять".into(),
            ));
        }
    }

    price_or_full
}

fn report_storage_items(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) {
    let catalog_all = state.lock().catalog_all.clone();

    let mut ah_ids = Vec::new();

    if let Some(contents) = crate::safe_inv::try_contents(bot) {
        for i in 0..STORAGE_AH_SLOTS {
            if let Some(data) =
                contents.get(i).and_then(present)
            {
                if let Some(info) =
                    match_slot_info(
                        bot,
                        data,
                        &catalog_all,
                        &boot.go_type,
                    )
                {
                    ah_ids.push(info.id);
                }
            }
        }
    }

    emit(&WorkerMsg {
        name: "items".into(),
        username: Some(boot.username.clone()),
        items: Some(json!(ah_ids)),
        ..crate::empty_msg()
    });

    let inv_ids =
        sell::inventory_catalog_ids(bot, boot, state);

    emit(&WorkerMsg {
        name: "inventory".into(),
        username: Some(boot.username.clone()),
        data: Some(json!(inv_ids)),
        ..crate::empty_msg()
    });
}

// Как JS waitStorageGuiSettle:
// поллим GUI, пока content не изменится
// или не наступит timeout.
//
// min_settle = 200мс
// max = WINDOW_DELAY
async fn wait_storage_gui_settle(
    bot: &Client,
    content_before: &str,
) -> GuiSettleResult {
    let t0 = Instant::now();

    let min_settle =
        Duration::from_millis(200);

    let max_settle =
        delay::duration("WINDOW_DELAY");

    let poll_interval =
        Duration::from_millis(100);

    loop {
        if t0.elapsed() >= max_settle {
            tracing::debug!(
                "wait_storage_gui_settle → timeout"
            );

            return GuiSettleResult::Timeout;
        }

        if crate::safe_inv::try_id(bot).is_none() {
            return GuiSettleResult::NoWindow;
        }

        let content_now =
            storage_content_key(bot);

        if t0.elapsed() >= min_settle
            && content_now != content_before
        {
            tracing::debug!(
                "wait_storage_gui_settle → changed"
            );

            return GuiSettleResult::Changed;
        }

        tokio::time::sleep(poll_interval).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuiSettleResult {
    Changed,
    NoWindow,
    Timeout,
}
const STORAGE_DUMP_MAX: u32 = 40;

/// Снимок открытого окна хранилища в `dumps/storage-window.jsonl` (одна JSON-строка на снимок):
/// заголовок, id окна, состояние бота и все слоты (предмет, количество, лор, custom_data).
/// Пароль и прокси в дамп не попадают. Не более `STORAGE_DUMP_MAX` снимков за запуск бота.
pub(crate) fn dump_storage_window(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>, reason: &str) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNT: AtomicU32 = AtomicU32::new(0);
    // снимок «после аренды» пишем всегда: он нужен, чтобы увидеть раскладку окна с арендованными слотами
    if reason != "after_rent" && COUNT.fetch_add(1, Ordering::Relaxed) >= STORAGE_DUMP_MAX {
        return;
    }

    let slots: Vec<serde_json::Value> = match crate::safe_inv::try_contents(bot) {
        Some(contents) => contents
            .iter()
            .enumerate()
            .map(|(i, st)| match present(st) {
                Some(d) => json!({
                    "slot": i,
                    "item": crate::item_parse::item_minecraft_name(d),
                    "count": d.count,
                    "price": ah_price(d),
                    "lore": crate::item_parse::lore_strings(d),
                    "custom_data": crate::item_parse::custom_data_json(d),
                }),
                None => json!({ "slot": i, "item": null }),
            })
            .collect(),
        None => Vec::new(),
    };

    let rec = {
        let s = state.lock();
        json!({
            "ts": crate::item_parse::chrono_like_now(),
            "nick": boot.username,
            "anarchy": boot.anarchy,
            "reason": reason,
            "window_id": s.last_window_id,
            "window_title": s.last_window_title,
            "storage_win_id": s.storage_win_id,
            "need_reset": s.need_reset,
            "need_sell": s.need_sell,
            "reset_click_ms_ago": s.last_reset_click_at.map(|t| t.elapsed().as_millis() as u64),
            "slots_total": slots.len(),
            "slots": slots,
        })
    };

    let dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("dumps");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("storage-window.jsonl");
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "{rec}");
            info!("хранилище → дамп окна ({reason}) → {}", path.display());
        }
        Err(e) => warn!("хранилище → не смог записать дамп окна: {e}"),
    }
}
