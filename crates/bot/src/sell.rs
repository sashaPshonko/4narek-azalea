//! Продажа: listing id 0–4 + /ah sell + clan после.
//! Задержки — `delay.json` / SELL_* как 4narek-old.

use std::{sync::Arc, time::Duration};

use azalea::prelude::*;
use azalea_inventory::ItemStack;
use ipc::BotBoot;
use parking_lot::Mutex;
use serde_json::json;
use tracing::{info, warn};

use crate::{
    afk, clan, delay,
    item_parse::{
        all_enchant_labels, durability_percent, get_slot_kind, match_slot_info, present, SlotKind,
    },
    listing,
    state::{BotState, SellAck},
    trash, warp,
};

const HOTBAR_START: usize = 36;
const HOTBAR_END: usize = 44;

pub async fn sell_pass(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    {
        let mut s = state.lock();
        s.need_sell = false;
    }
    state.lock().storage_presence_sent = false;
    if crate::safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }

    let catalog_all = state.lock().catalog_all.clone();
    if catalog_all.is_empty() {
        warn!("sell → каталог пуст (ждём Go prices)");
        return;
    }

    if state.lock().afk {
        afk::un_afk(bot).await;
        state.lock().afk = false;
    }

    if trash::drop_trash(bot, boot, state).await {
        trash::do_rtp_if_needed(bot, state).await;
        return;
    }

    if state.lock().owner_ban_drain {
        clan::drain_treasury_and_leave(bot, boot, state, false).await;
        return;
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

    for idx in HOTBAR_START..=HOTBAR_END {
        if sold >= 5 {
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

        let mut stop_hotbar = false;
        for attempt in 1..=delay::SELL_SLOT_MAX_ATTEMPTS {
            if slot_gone() || state.lock().has_dangerous_trash {
                break;
            }
            if let Some(np) = state.lock().need_sell_price.take() {
                sell_price = np;
            }

            let alloc = {
                let mut s = state.lock();
                listing::alloc_local(
                    &mut s.listings,
                    sell_price,
                    &info.id,
                    enchants.clone(),
                    Some(dur),
                    &boot.username,
                )
            };
            let Some((listing_id, list_price)) = alloc else {
                warn!("sell → нет свободного listing id 0–4");
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
            crate::cmd::say(bot, format!("/ah sell {price_i}"));

            let mut ack = wait_sell_ack(
                state,
                Duration::from_millis(delay::SELL_LIST_ACK_TIMEOUT_MS),
            )
            .await;
            // old: timeout → retry если needPrice / ok если слот пуст / иначе confirm
            if ack == SellAck::Waiting || ack == SellAck::Idle {
                if state.lock().need_sell_price.is_some() {
                    ack = SellAck::Retry;
                } else if slot_gone() {
                    ack = SellAck::Ok;
                } else {
                    ack = SellAck::Confirm;
                }
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
        if stop_hotbar {
            break;
        }
    }

    if sold > 0 {
        info!("продажа → выставлено {sold}");
    }
    // как JS finally: сброс needSell после прохода продажи
    state.lock().need_sell = false;

    delay::rnd("BASE_DELAY").await;
    clan::after_sell_balance(bot, boot, state).await;
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
async fn fill_hotbar_from_backpack(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    if state.lock().has_dangerous_trash {
        return;
    }
    // сначала выкинуть trash с хотбара, потом забить пустые
    let _ = trash::drop_trash(bot, boot, state).await;
    for _ in 0..9 {
        if !move_one_backpack_to_hotbar(bot, boot, state).await {
            break;
        }
    }
}

async fn move_one_backpack_to_hotbar(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> bool {
    let catalog_all = state.lock().catalog_all.clone();
    let slots = player_inventory_slots(bot);
    let mut target_hot: Option<usize> = None;
    for i in HOTBAR_START..=HOTBAR_END {
        let Some(data) = slots.get(i).and_then(present) else {
            target_hot = Some(i);
            break;
        };
        // пустой или trash — можно заменить; trade/foreign — занят
        match get_slot_kind(bot, data, &catalog_all, &boot.go_type) {
            SlotKind::Trash => {
                target_hot = Some(i);
                break;
            }
            SlotKind::Trade(_) | SlotKind::Foreign { .. } => continue,
        }
    }
    let Some(hot) = target_hot else {
        return false;
    };
    // trash на целевом слоте — выкинуть перед переносом
    if let Some(data) = slots.get(hot).and_then(present) {
        if matches!(
            get_slot_kind(bot, data, &catalog_all, &boot.go_type),
            SlotKind::Trash
        ) {
            bot.set_selected_hotbar_slot((hot - HOTBAR_START) as u8);
            if crate::safe_inv::try_close(bot) {
                delay::rnd("POLL").await;
            }
            if let Some(handle) = bot.open_inventory() {
                handle.left_click(hot);
                delay::rnd("POLL").await;
                handle.click(azalea_inventory::operations::PickupClick::LeftOutside);
                delay::rnd("POLL").await;
                drop(handle);
            }
            delay::rnd("500MS").await;
        }
    }
    let slots = player_inventory_slots(bot);
    for idx in 9..36 {
        let Some(data) = slots.get(idx).and_then(present) else {
            continue;
        };
        if match_slot_info(bot, data, &catalog_all, &boot.go_type).is_none() {
            continue;
        }
        let Some(inv) = crate::safe_inv::try_handle(bot) else {
            return false;
        };
        inv.left_click(idx);
        delay::rnd("POLL").await;
        inv.left_click(hot);
        info!("продажа → перенёс в хотбар");
        delay::rnd("HOTBAR_DELAY").await;
        return true;
    }
    false
}

/// Как JS sellSlotIsEmpty: пусто или trash/не-trade → слот «ушёл».
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

/// Id каталога в инвентаре (слоты 9–40), как JS `inventory: [id, id, …]` → Go per-SKU.
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
