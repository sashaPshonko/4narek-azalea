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

    for idx in 9..=44 {
        let Some(data) = slots.get(idx).and_then(present) else {
            continue;
        };
        match get_slot_kind(bot, data, &catalog, &boot.go_type) {
            SlotKind::Trade(_) | SlotKind::Foreign { .. } => continue,
            SlotKind::Trash => {}
        }
        info!("мусор → выкинул");
        bot.set_selected_hotbar_slot(((idx.saturating_sub(36)).min(8)) as u8);
        if safe_inv::try_close(bot) {
            delay::rnd("POLL").await;
        }
        if !safe_inv::ready(bot) {
            return false;
        }
        if let Some(handle) = bot.open_inventory() {
            handle.left_click(idx);
            delay::rnd("POLL").await;
            handle.click(azalea_inventory::operations::PickupClick::LeftOutside);
            delay::rnd("POLL").await;
            drop(handle);
        }
        delay::rnd("500MS").await;
        if state.lock().has_dangerous_trash {
            warn!("мусор → опасный, нужен RTP");
            return true;
        }
    }
    false
}

pub async fn do_rtp_if_needed(bot: &Client, state: &Arc<Mutex<BotState>>) {
    if !state.lock().need_rtp && !state.lock().has_dangerous_trash {
        return;
    }
    crate::cmd::say(bot, "/rtp");
    delay::rnd("RTP").await;
    state.lock().need_rtp = false;
    state.lock().has_dangerous_trash = false;
}

pub fn is_empty_stack(slot: &ItemStack) -> bool {
    present(slot).is_none()
}
