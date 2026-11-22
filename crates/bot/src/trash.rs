//! Выброс мусора / RTP. Категории как old getSlotInfo: trash vs foreign.

use std::sync::Arc;

use azalea::prelude::*;
use azalea_inventory::ItemStack;
use ipc::BotBoot;
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::{
    delay,
    item_parse::{get_slot_kind, present, SlotKind},
    state::BotState,
};

/// Выкинуть trash; foreign не трогаем. true → нужен RTP.
pub async fn drop_trash(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) -> bool {
    if state.lock().has_dangerous_trash {
        return true;
    }
    let catalog = state.lock().catalog_all.clone();
    let catalog = if catalog.is_empty() {
        state.lock().catalog.clone()
    } else {
        catalog
    };

    use crate::safe_inv;
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return false;
    };

    // Какие слоты — мусор (решаем по снимку до открытия инвентаря).
    let trash: Vec<usize> = (9..=44)
        .filter(|&idx| {
            slots.get(idx).and_then(present).is_some_and(|data| {
                matches!(get_slot_kind(bot, data, &catalog, &boot.go_type), SlotKind::Trash)
            })
        })
        .collect();
    if trash.is_empty() {
        return false;
    }

    if safe_inv::try_close(bot) {
        delay::rnd("POLL").await;
    }
    if !safe_inv::ready(bot) {
        return false;
    }

    // Как живой игрок: открыл инвентарь (E) один раз, выбросил весь мусор подряд, закрыл один раз.
    // Раньше бот открывал и закрывал инвентарь на каждый предмет (лишние ContainerClose 0 между выбросами)
    // и перед каждым выбросом переключал слот в руке — клиенту для выброса из инвентаря это не нужно.
    let Some(handle) = bot.open_inventory() else {
        return false;
    };
    delay::rnd("HOTBAR_DELAY").await;

    let mut dangerous = false;
    for (n, idx) in trash.into_iter().enumerate() {
        if n > 0 {
            // пауза между предметами, как у человека (≈0,5–1,5 с)
            delay::rnd("HOTBAR_DELAY").await;
        }
        // слот мог опустеть, пока ждали
        let still_there = safe_inv::try_player_slots(bot)
            .is_some_and(|now| now.get(idx).and_then(present).is_some());
        if !still_there {
            continue;
        }
        info!("мусор → выкинул");
        handle.left_click(idx);
        delay::rnd("SELL_HOTBAR").await;
        handle.click(azalea_inventory::operations::PickupClick::LeftOutside);
        delay::rnd("POLL").await;
        if state.lock().has_dangerous_trash {
            warn!("мусор → опасный, нужен RTP");
            dangerous = true;
            break;
        }
    }

    // от последнего клика до закрытия у живого игрока ≈0,5 с (записи бот-вью)
    tokio::time::sleep(std::time::Duration::from_millis(350 + rand::random::<u64>() % 500)).await;
    drop(handle); // ContainerClose 0 — один раз на весь выброс
    dangerous
}

pub async fn do_rtp_if_needed(bot: &Client, state: &Arc<Mutex<BotState>>) {
    if !state.lock().need_rtp && !state.lock().has_dangerous_trash {
        return;
    }
    delay::rnd("BASE_DELAY").await;
    crate::cmd::say(bot, "/rtp");
    delay::rnd("RTP").await;
    state.lock().need_rtp = false;
    state.lock().has_dangerous_trash = false;
}

pub fn is_empty_stack(slot: &ItemStack) -> bool {
    present(slot).is_none()
}
