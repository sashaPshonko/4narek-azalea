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

    // sync listing ids по ценам в слотах 0–4
    let prices = storage_prices(bot);
    {
        let mut s = state.lock();
        s.listings.sync_from_storage_prices(&prices);
        listing::mirror_op(
            &boot.username,
            "sync",
            json!({ "prices": prices }),
        );
    }

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

    // как JS: зашли в хранилище → needReset снят (триггер уже отработал)
    state.lock().need_reset = false;

    // presence: ids на АХ + инвентарь
    report_storage_items(bot, boot, state);

    // сброс раз в 60с (клик 52) если слот 0 занят
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

    // CD перестановки
    if let Some(until) = state.lock().storage_unlist_until {
        if Instant::now() < until {
            let left = until.saturating_duration_since(Instant::now());
            tracing::debug!("хранилище → CD перестановки ещё {}с", left.as_secs());
            tokio::time::sleep(left.min(Duration::from_secs(2))).await;
            return StorageResult::Stay;
        }
    }

    let unlist = find_slot_to_unlist(bot, boot, state);
    if let Some((slot, reason)) = unlist {
        let storage_key = storage_content_key(bot);
        // тот же слот + тот же GUI ≥2 раза → reload 50 (призрак «уже купили»)
        {
            let mut s = state.lock();
            if slot as i32 == s.storage_stale_unlist_slot
                && storage_key == s.storage_stale_unlist_key
                && s.storage_stale_unlist_streak >= 2
            {
                info!("хранилище → залип слот {slot} ×{} → reload", s.storage_stale_unlist_streak);
                s.storage_stale_unlist_streak = 0;
                s.storage_stale_unlist_slot = -1;
                s.storage_stale_unlist_key.clear();
                drop(s);
                reload_storage(bot).await;
                return StorageResult::Stay;
            }
        }

        info!("хранилище → снимаю #{slot} ({reason})");
        state.lock().need_sell = true;
        let content_before = storage_key;
        if !crate::safe_inv::try_left_click(bot, slot) {
            return StorageResult::Closed;
        }
        delay::rnd("UNLIST").await;

        let content_after = storage_content_key(bot);
        {
            let mut s = state.lock();
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
        return StorageResult::Unlisted;
    }

    {
        let mut s = state.lock();
        s.storage_stale_unlist_streak = 0;
        s.storage_stale_unlist_slot = -1;
        s.storage_stale_unlist_key.clear();
    }

    // продать если needSell / полный инв
    let should_sell = state.lock().need_sell || sell::inventory_full(bot, boot, state);
    if should_sell && sell::count_our_items(bot, boot, state) > 0 {
        info!("хранилище → иду продавать");
        let _ = crate::safe_inv::try_close(bot);
        delay::rnd("500MS").await;
        sell::sell_pass(bot, boot, state).await;
        state.lock().need_sell = false;
        state.lock().mark_unlist_cycle_done();
        return StorageResult::Sold;
    }

    // КД-снятие, а снимать нечего — сдвигаем таймер только если слот 0 пуст (как JS)
    if state.lock().unlist_cycle_due() {
        let slot0_empty = crate::safe_inv::try_contents(bot)
            .map(|c| c.get(0).and_then(present).is_none())
            .unwrap_or(true);
        if slot0_empty {
            state.lock().mark_unlist_cycle_done();
            info!("хранилище → КД-снятие: пусто/нечего, назад в АХ");
        }
    }

    // назад в АХ
    tracing::debug!("хранилище → назад в АХ");
    if crate::safe_inv::try_left_click(bot, SLOT_BACK_AH) {
        delay::rnd("WINDOW_DELAY").await;
    }
    StorageResult::BackToAh
}

pub async fn reload_storage(bot: &Client) {
    if !crate::safe_inv::try_left_click(bot, SLOT_RELOAD) {
        return;
    }
    info!("хранилище → обновляю");
    delay::rnd("BASE_DELAY").await;
}

fn storage_content_key(bot: &Client) -> String {
    let Some(contents) = crate::safe_inv::try_contents(bot) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (i, slot) in contents.iter().enumerate().take(STORAGE_AH_SLOTS) {
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
    let clear_all = {
        let s = state.lock();
        inv_full || s.unlist_cycle_due()
    };

    let (catalog_all, listings_snap) = {
        let s = state.lock();
        (s.catalog_all.clone(), s.listings.export_state())
    };
    let mem_rows = listings_snap
        .get("listings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut price_or_full: Option<(usize, String)> = None;

    for i in (0..STORAGE_AH_SLOTS).rev() {
        let Some(data) = contents.get(i).and_then(present) else {
            continue;
        };
        // как JS: foreign → skip; !hit (trash) → снять; trade → цена/clearAll
        match get_slot_kind(bot, data, &catalog_all, &boot.go_type) {
            SlotKind::Foreign { .. } => continue,
            SlotKind::Trash => {
                return Some((i, "мусор (нет в каталоге)".into()));
            }
            SlotKind::Trade(_) => {}
        }
        let Some(price) = ah_price(data) else {
            return Some((i, "мусор (не читается цена)".into()));
        };
        let listing_id = (price as i64 % 10) as u8;
        let mem = mem_rows.iter().find(|r| {
            r.get("listingId").and_then(|v| v.as_u64()) == Some(listing_id as u64)
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
                durability: row.get("durability").and_then(|v| v.as_f64()),
                price: row.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0),
            };
            if let Some(expected) = expected_sell_from_meta(&catalog_all, &meta) {
                if !ListingMemory::prices_aligned(price, expected) && price_or_full.is_none() {
                    price_or_full = Some((
                        i,
                        format!("цена {price} ≠ {expected}"),
                    ));
                    continue;
                }
            }
        }
        if clear_all && price_or_full.is_none() {
            let reason = if inv_full {
                "инвентарь ≥27 — снять"
            } else {
                "КД-снятие — снять"
            };
            price_or_full = Some((i, reason.into()));
        }
    }
    price_or_full
}

fn report_storage_items(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    let catalog_all = state.lock().catalog_all.clone();
    let mut ah_ids = Vec::new();
    if let Some(contents) = crate::safe_inv::try_contents(bot) {
        for i in 0..STORAGE_AH_SLOTS {
            if let Some(data) = contents.get(i).and_then(present) {
                if let Some(info) = match_slot_info(bot, data, &catalog_all, &boot.go_type) {
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
    let inv_ids = sell::inventory_catalog_ids(bot, boot, state);
    emit(&WorkerMsg {
        name: "inventory".into(),
        username: Some(boot.username.clone()),
        data: Some(json!(inv_ids)),
        ..crate::empty_msg()
    });
}
