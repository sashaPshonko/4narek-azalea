//! Своя броня бота: на каждом слоте брони надет незерит, «позорный».
//!
//! «Позорная» броня для надевания задана здесь жёстко и отдельно от каталога: незеритовый
//! кусок нужного типа с Unbreaking ниже 5 и Protection ниже 5 (любые уровни ниже тоже
//! подходят, чтобы можно было купить дешевле), без Mending и Thorns.
//! SKU `*-позор-1.21` в items_config.json это другое: он не менялся и этим кодом не
//! используется (то, как Go торгует позором, и то, что бот надевает на себя, независимы).
//!
//! **АХ** (внутри `ah::best_buy_slot`): если слот брони не занят незеритом нужного типа
//! и позорной такого типа нет в инвентаре, то первый встретившийся позорный лот этого типа
//! покупается без проверки цен и книги. `buying`/`buy` в оркестратор и Go не отправляются.
//!
//! **sell_pass:** если слот не занят незеритом, а позорная такого типа лежит в инвентаре,
//! меняем: позорную в слот, вытесненную вещь (кожаная и т. п.) выбрасываем.
//!
//! Протокол: обычные клики по окну игрока (`container_id` = 0, Pickup). Слоты меню:
//! 5–8 шлем / нагрудник / штаны / ботинки, 9–35 инвентарь, 36–44 хотбар.
//! 1) клик по вещи в инвентаре: она на курсоре; 2) клик по слоту брони: сервер кладёт
//! или меняет местами (старая вещь окажется на курсоре); 3) клик за пределы окна
//! (слот −999) выбрасывает то, что осталось на курсоре.

use std::sync::Arc;

use azalea::entity::inventory::Inventory;
use azalea::prelude::*;
use azalea_inventory::{operations::PickupClick, ItemStack};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::{
    delay,
    item_parse::{all_enchant_labels, enchant_names_match, item_minecraft_name, present},
    safe_inv,
    state::{self, BotState},
};

/// Слоты брони в меню игрока: 5 шлем, 6 нагрудник, 7 штаны, 8 ботинки.
pub const ARMOR_SLOTS: [(usize, &str, &str); 4] = [
    (5, "netherite_helmet", "шлем"),
    (6, "netherite_chestplate", "нагрудник"),
    (7, "netherite_leggings", "штаны"),
    (8, "netherite_boots", "ботинки"),
];

/// Слот брони для названия предмета (только незеритовая броня).
pub fn armor_slot_for(kind: &str) -> Option<usize> {
    ARMOR_SLOTS
        .iter()
        .find(|(_, k, _)| *k == kind)
        .map(|(slot, _, _)| *slot)
}

fn armor_kind_for_slot(slot: usize) -> Option<&'static str> {
    ARMOR_SLOTS
        .iter()
        .find(|(s, _, _)| *s == slot)
        .map(|(_, k, _)| *k)
}

fn armor_name_ru(slot: usize) -> &'static str {
    ARMOR_SLOTS
        .iter()
        .find(|(s, _, _)| *s == slot)
        .map(|(_, _, ru)| *ru)
        .unwrap_or("броня")
}

fn enchant_level(labels: &[(String, i32)], name: &str) -> i32 {
    labels
        .iter()
        .filter(|(n, _)| enchant_names_match(n, name))
        .map(|(_, l)| *l)
        .max()
        .unwrap_or(0)
}

/// Набор брони для надевания по зачарованиям (не SKU каталога): Unbreaking ниже 5 и Protection ниже 5
/// (любой уровень, включая 0), без Mending и Thorns.
pub fn pozor_by_enchants(labels: &[(String, i32)]) -> bool {
    enchant_level(labels, "minecraft:unbreaking") < 5
        && enchant_level(labels, "minecraft:protection") < 5
        && enchant_level(labels, "minecraft:mending") == 0
        && enchant_level(labels, "minecraft:thorns") == 0
}

/// Слоты брони, не занятые незеритом нужного типа (пусто или любая другая вещь).
/// `kind_at(slot)` — название предмета в слоте меню игрока (None = пусто).
pub fn free_armor_slots(kind_at: impl Fn(usize) -> Option<String>) -> Vec<usize> {
    ARMOR_SLOTS
        .iter()
        .filter(|(slot, kind, _)| kind_at(*slot).as_deref() != Some(*kind))
        .map(|(slot, _, _)| *slot)
        .collect()
}

/// Лот АХ — позорная броня под один из нужных слотов? Возвращает слот брони.
pub fn wear_slot_for_lot(kind: &str, labels: &[(String, i32)], needed: &[usize]) -> Option<usize> {
    let slot = armor_slot_for(kind)?;
    (needed.contains(&slot) && pozor_by_enchants(labels)).then_some(slot)
}

fn player_kind_at(slots: &[ItemStack], idx: usize) -> Option<String> {
    slots.get(idx).and_then(present).map(item_minecraft_name)
}

/// Индекс (9..=44) позорной вещи нужного типа в инвентаре.
fn find_pozor_in_inventory(bot: &Client, slots: &[ItemStack], armor_slot: usize) -> Option<usize> {
    let kind = armor_kind_for_slot(armor_slot)?;
    (9..=44).find(|idx| {
        let Some(data) = slots.get(*idx).and_then(present) else {
            return false;
        };
        item_minecraft_name(data) == kind && pozor_by_enchants(&all_enchant_labels(bot, data))
    })
}

// ============================================================
// Какие слоты нужны (кэш для окна АХ)
// ============================================================

/// Слоты, под которые надо купить позорную броню: слот не занят незеритом, и позорной такого
/// типа нет в инвентаре. Вызывать при закрытом окне (пока открыт контейнер, меню игрока
/// может быть устаревшим).
pub fn refresh_missing(bot: &Client, state: &Arc<Mutex<BotState>>) {
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return;
    };
    let missing: Vec<usize> = free_armor_slots(|s| player_kind_at(&slots, s))
        .into_iter()
        .filter(|s| find_pozor_in_inventory(bot, &slots, *s).is_none())
        .collect();
    state.lock().wear_missing = missing;
}

/// Запомнить, что покупается броня «для себя»; возвращает подпись для лога.
pub fn begin_purchase(state: &Arc<Mutex<BotState>>, armor_slot: usize, price: f64) -> String {
    state.lock().buying_item = state::BuyingItem {
        price,
        wear: Some(state::WearBuy { armor_slot }),
        ..Default::default()
    };
    format!(
        "АХ → позорный {} для себя · {}",
        armor_name_ru(armor_slot),
        crate::money::money(price)
    )
}

/// После «Вы успешно купили»: если это броня для себя, слот убирается из нужных и
/// будится sell_pass (там надевание). Возвращает true (тогда `buy` в Go не слать).
pub fn on_purchase_confirmed(state: &Arc<Mutex<BotState>>) -> bool {
    let mut st = state.lock();
    let Some(w) = st.buying_item.wear.take() else {
        return false;
    };
    st.buying_item = state::BuyingItem::default();
    st.wear_missing.retain(|s| *s != w.armor_slot);
    st.need_sell = true;
    true
}

// ============================================================
// Надевание (в sell_pass)
// ============================================================

pub async fn equip_all(bot: &Client, state: &Arc<Mutex<BotState>>) {
    for (armor_slot, expect, ru) in ARMOR_SLOTS {
        let Some(slots) = safe_inv::try_player_slots(bot) else {
            return;
        };
        if player_kind_at(&slots, armor_slot).as_deref() == Some(expect) {
            continue; // слот уже занят незеритом
        }
        let Some(idx) = find_pozor_in_inventory(bot, &slots, armor_slot) else {
            continue;
        };

        if safe_inv::try_close(bot) {
            delay::rnd("POLL").await;
        }
        let Some(inv) = safe_inv::try_handle(bot) else {
            continue;
        };
        if inv.id() != 0 {
            continue;
        }

        info!("броня → надеваю позорный {ru} (слот {idx} → {armor_slot})");
        delay::rnd("HOTBAR_DELAY").await;
        inv.left_click(idx);
        delay::rnd("SELL_HOTBAR").await;
        inv.left_click(armor_slot);

        let mut done = false;
        for _ in 0..5 {
            delay::rnd("POLL").await;
            if let Some(now) = safe_inv::try_player_slots(bot) {
                if player_kind_at(&now, armor_slot).as_deref() == Some(expect) {
                    done = true;
                    break;
                }
            }
        }

        // что осталось на курсоре
        let carried = bot.get_component::<Inventory>().map(|inv| inv.carried.clone());
        if let Some(data) = carried.as_ref().and_then(present) {
            if !done && item_minecraft_name(data) == expect {
                // слот не принял: вернуть позорную на место
                delay::rnd("SELL_HOTBAR").await;
                inv.left_click(idx);
                warn!("броня → {ru}: слот не принял, вернул позорную в инвентарь");
            } else {
                // вытесненная старая вещь (кожаная и т. п.): выбросить
                delay::rnd("SELL_HOTBAR").await;
                inv.click(PickupClick::LeftOutside);
                info!("броня → выбросил старую вещь ({})", item_minecraft_name(data));
                delay::rnd("POLL").await;
            }
        }

        if done {
            info!("броня → надел позорный {ru}");
            delay::rnd("HOTBAR_DELAY").await;
        } else {
            warn!("броня → {ru} не надет (повторю в следующей прогулке-продаже)");
        }
    }

    refresh_missing(bot, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armor_slots_by_kind() {
        assert_eq!(armor_slot_for("netherite_helmet"), Some(5));
        assert_eq!(armor_slot_for("netherite_chestplate"), Some(6));
        assert_eq!(armor_slot_for("netherite_leggings"), Some(7));
        assert_eq!(armor_slot_for("netherite_boots"), Some(8));
        assert_eq!(armor_slot_for("diamond_helmet"), None);
        assert_eq!(armor_slot_for("leather_helmet"), None);
        assert_eq!(armor_slot_for("netherite_sword"), None);
    }

    #[test]
    fn free_slots_are_not_netherite() {
        // шлем незеритовый (любой), нагрудник кожаный, штаны пусты, ботинки алмазные
        let kind = |s: usize| match s {
            5 => Some("netherite_helmet".to_string()),
            6 => Some("leather_chestplate".to_string()),
            8 => Some("diamond_boots".to_string()),
            _ => None,
        };
        assert_eq!(free_armor_slots(kind), vec![6, 7, 8]);
        // не тот тип незерита в слоте тоже считается незанятым
        let wrong = |s: usize| (s == 5).then(|| "netherite_boots".to_string());
        assert!(free_armor_slots(wrong).contains(&5));
        // все надеты
        let all = |s: usize| armor_kind_for_slot(s).map(str::to_string);
        assert!(free_armor_slots(all).is_empty());
    }

    fn labels(v: &[(&str, i32)]) -> Vec<(String, i32)> {
        v.iter().map(|(n, l)| (n.to_string(), *l)).collect()
    }

    #[test]
    fn pozor_set_is_below_five_and_five() {
        // классика позора: Unbreaking 4 + Protection 4
        assert!(pozor_by_enchants(&labels(&[("minecraft:unbreaking", 4), ("minecraft:protection", 4)])));
        // нижние уровни и совсем без чар: подходят (так дешевле)
        assert!(pozor_by_enchants(&labels(&[("minecraft:unbreaking", 3), ("minecraft:protection", 2)])));
        assert!(pozor_by_enchants(&labels(&[("minecraft:protection", 4)])));
        assert!(pozor_by_enchants(&[]));
        // 5 уже не позор: ни Unbreaking, ни Protection
        assert!(!pozor_by_enchants(&labels(&[("minecraft:unbreaking", 5), ("minecraft:protection", 4)])));
        assert!(!pozor_by_enchants(&labels(&[("minecraft:unbreaking", 4), ("minecraft:protection", 5)])));
        assert!(!pozor_by_enchants(&labels(&[("minecraft:unbreaking", 5), ("minecraft:protection", 5)])));
        // Mending и Thorns по-прежнему исключают
        assert!(!pozor_by_enchants(&labels(&[
            ("minecraft:unbreaking", 4),
            ("minecraft:protection", 4),
            ("minecraft:mending", 1),
        ])));
        assert!(!pozor_by_enchants(&labels(&[("minecraft:thorns", 3)])));
        // лишние чары (дыхание и т. п.) позор не отменяют
        assert!(pozor_by_enchants(&labels(&[
            ("minecraft:unbreaking", 4),
            ("minecraft:protection", 4),
            ("minecraft:respiration", 3),
        ])));
    }

    #[test]
    fn lot_is_wanted_only_if_pozor_and_slot_needed() {
        let pozor = labels(&[("minecraft:unbreaking", 4), ("minecraft:protection", 4)]);
        let better = labels(&[("minecraft:unbreaking", 4), ("minecraft:protection", 5)]);
        let needed = [5, 8];
        assert_eq!(wear_slot_for_lot("netherite_helmet", &pozor, &needed), Some(5));
        assert_eq!(wear_slot_for_lot("netherite_boots", &pozor, &needed), Some(8));
        // слот не нужен
        assert_eq!(wear_slot_for_lot("netherite_chestplate", &pozor, &needed), None);
        // не позорный набор
        assert_eq!(wear_slot_for_lot("netherite_helmet", &better, &needed), None);
        // не броня
        assert_eq!(wear_slot_for_lot("netherite_sword", &pozor, &needed), None);
    }
}
