//! Хранилище АХ: sync listing, снятие, reload, назад в АХ.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use serde_json::json;
use tracing::info;

use crate::{
    delay,
    item_parse::{ah_price, get_slot_kind, match_slot_info, present, SlotKind},
    listing::{self, expected_sell_from_meta, ListingMemory},
    sell,
    state::BotState,
};

const STORAGE_AH_SLOTS: usize = 5;
const SLOT_BACK_AH: usize = 46;
const SLOT_RELOAD: usize = 50;
const SLOT_RESET: usize = 52;

pub async fn handle_storage(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> StorageResult {
    delay::rnd("GLASS").await;

    match crate::safe_inv::try_id(bot) {
        Some(id) if id != 0 => {}
        _ => return StorageResult::Closed,
    }

    // Синхронизация listing ids по ценам в слотах 0–4.
    let prices = storage_prices(bot);

    {
        let mut s = state.lock();

        s.listings.sync_from_storage_prices(&prices);

        listing::mirror_op(
            &boot.username,
            "sync",
            json!({
                "prices": prices
            }),
        );
    }

    // Если нужен reload АХ, делаем его и остаёмся в storage.
    if state.lock().need_reload_ah {
        {
            let mut s = state.lock();

            s.need_reload_ah = false;
            s.storage_stale_unlist_streak = 0;
            s.storage_stale_unlist_slot = -1;
            s.storage_stale_unlist_key.clear();
        }

        reload_storage(bot).await;
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
    // Это НЕ КД-снятие.
    let need_reset = {
        let mut s = state.lock();

        if s.last_storage_reset.elapsed() > Duration::from_secs(60) {
            s.last_storage_reset = Instant::now();
            true
        } else {
            false
        }
    };

    if need_reset {
        let has0 = crate::safe_inv::try_contents(bot)
            .map(|c| c.get(0).and_then(present).is_some())
            .unwrap_or(false);

        if has0 {
            info!("хранилище → сброс витрины");

            if !crate::safe_inv::try_left_click(bot, SLOT_RESET) {
                return StorageResult::Closed;
            }

            delay::rnd("WINDOW_DELAY").await;
            return StorageResult::Stay;
        }
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

                reload_storage(bot).await;
                return StorageResult::Stay;
            }
        }

        info!("хранилище → снимаю #{slot} ({reason})");

        let content_before = storage_key;

        if !crate::safe_inv::try_left_click(bot, slot) {
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

    if should_sell && sell::count_our_items(bot, boot, state) > 0 {
        info!("хранилище → витрина очищена, иду продавать");

        let _ = crate::safe_inv::try_close(bot);

        delay::rnd("500MS").await;

        sell::sell_pass(bot, boot, state).await;

        state.lock().need_sell = false;

        return StorageResult::Sold;
    }

    // Назад в АХ.
    tracing::debug!("хранилище → назад в АХ");

    if crate::safe_inv::try_left_click(bot, SLOT_BACK_AH) {
        delay::rnd("WINDOW_DELAY").await;
    }

    StorageResult::BackToAh
}

pub async fn reload_storage(bot: &Client) {
    let content_before = storage_content_key(bot);

    if !crate::safe_inv::try_left_click(bot, SLOT_RELOAD) {
        return;
    }

    info!("хранилище → обновляю");

    delay::rnd("BASE_DELAY").await;

    // Ждём обновления GUI.
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

fn find_slot_to_unlist(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> Option<(usize, String)> {
    let contents = crate::safe_inv::try_contents(bot)?;

    let inv_full = sell::inventory_full(bot, boot, state);

    let (catalog_all, listings_snap) = {
        let s = state.lock();

        (
            s.catalog_all.clone(),
            s.listings.export_state(),
        )
    };

    let mem_rows = listings_snap
        .get("listings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut price_or_full: Option<(usize, String)> = None;

    // Идём с 4 к 0, как в старой логике.
    for i in (0..STORAGE_AH_SLOTS).rev() {
        let Some(data) = contents.get(i).and_then(present) else {
            continue;
        };

        // Foreign → пропускаем.
        // Trash → снимаем.
        // Trade → проверяем цену / снимаем при полном инвентаре.
        match get_slot_kind(
            bot,
            data,
            &catalog_all,
            &boot.go_type,
        ) {
            SlotKind::Foreign { .. } => continue,

            SlotKind::Trash => {
                return Some((
                    i,
                    "мусор (нет в каталоге)".into(),
                ));
            }

            SlotKind::Trade(_) => {}
        }

        let Some(price) = ah_price(data) else {
            return Some((
                i,
                "мусор (не читается цена)".into(),
            ));
        };

        let listing_id = (price as i64 % 10) as u8;

        let mem = mem_rows.iter().find(|r| {
            r.get("listingId")
                .and_then(|v| v.as_u64())
                == Some(listing_id as u64)
        });

        if let Some(row) = mem {
            let meta = listing::ListingMeta {
                listing_id,

                catalog_id: row
                    .get("catalogId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),

                enchants: row
                    .get("enchants")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),

                durability: row
                    .get("durability")
                    .and_then(|v| v.as_f64()),

                price: row
                    .get("price")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
            };

            if let Some(expected) =
                expected_sell_from_meta(&catalog_all, &meta)
            {
                if !ListingMemory::prices_aligned(
                    price,
                    expected,
                ) && price_or_full.is_none()
                {
                    price_or_full = Some((
                        i,
                        format!(
                            "цена {price} ≠ {expected}"
                        ),
                    ));

                    continue;
                }
            }
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