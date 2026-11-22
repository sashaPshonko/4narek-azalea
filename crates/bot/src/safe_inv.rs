//! Inventory без panic (как `try_y` для Position).
//! Azalea `get_inventory()` / `query_self<&Inventory>` падают, если компонента нет
//! (transfer / respawn / mid-GUI).

use azalea::container::ContainerHandleRef;
use azalea::entity::inventory::Inventory;
use azalea::prelude::*;
use azalea_inventory::{ItemStack, Menu};

pub fn ready(bot: &Client) -> bool {
    bot.get_component::<Inventory>().is_some()
}

pub fn try_handle(bot: &Client) -> Option<ContainerHandleRef> {
    bot.get_component::<Inventory>()
        .map(|inv| ContainerHandleRef::new(inv.id, bot.clone()))
}

pub fn try_id(bot: &Client) -> Option<i32> {
    bot.get_component::<Inventory>().map(|inv| inv.id)
}

/// Закрыть открытое окно, если Inventory жив. `true` = отправили close.
pub fn try_close(bot: &Client) -> bool {
    let Some(inv) = bot.get_component::<Inventory>() else {
        return false;
    };
    if inv.id == 0 {
        return false;
    }
    ContainerHandleRef::new(inv.id, bot.clone()).close();
    true
}

/// Закрыть инвентарь игрока (id 0) пакетом `ServerboundContainerClose { 0 }`.
///
/// Так делает ванильный клиент: открытие инвентаря (E) пакета не шлёт, а закрытие (E/Esc) шлёт. У настоящего
/// игрока такие закрытия — больше половины всех `close_window` в записях бот-вью (20 из 35, `windowId = 0`).
/// После кликов по инвентарю через `try_handle` (броня, перенос в хотбар) закрытия не было вообще; `try_close`
/// для id 0 ничего не отправляет. `true` = пакет ушёл.
pub fn close_player_inventory(bot: &Client) -> bool {
    let Some(inv) = bot.get_component::<Inventory>() else {
        return false;
    };
    if inv.id != 0 {
        return false;
    }
    ContainerHandleRef::new(0, bot.clone()).close();
    true
}

/// Слоты контейнера без инвентаря игрока — как `ContainerHandleRef::contents`.
pub fn try_contents(bot: &Client) -> Option<Vec<ItemStack>> {
    let inv = bot.get_component::<Inventory>()?;
    if inv.id == 0 {
        return None;
    }
    Some(inv.container_menu.as_ref()?.contents())
}

/// Активное меню целиком (контейнер или player).
pub fn try_menu(bot: &Client) -> Option<Menu> {
    let inv = bot.get_component::<Inventory>()?;
    if inv.id == 0 {
        Some(inv.inventory_menu.clone())
    } else {
        inv.container_menu.clone()
    }
}

/// Предмет на курсоре (взят кликом и ещё не положен).
pub fn try_carried(bot: &Client) -> Option<ItemStack> {
    bot.get_component::<Inventory>().map(|inv| inv.carried.clone())
}

pub fn try_player_slots(bot: &Client) -> Option<Vec<ItemStack>> {
    bot.get_component::<Inventory>()
        .map(|inv| inv.inventory_menu.slots())
}

/// Клики безопасны (не трогают expect в menu); нужен живой Inventory только для id.
pub fn try_left_click(bot: &Client, slot: usize) -> bool {
    let Some(h) = try_handle(bot) else {
        return false;
    };
    if h.id() == 0 {
        return false;
    }
    h.left_click(slot);
    true
}

/// То же окно, на основании которого принято решение (аналог `botKey` в JS-версии): id совпадает и это не меню игрока.
pub fn window_matches(current: i32, expected: i32) -> bool {
    current != 0 && current == expected
}

/// Клик только если сейчас открыто окно `expect_id`. Между решением и кликом бот ждёт (задержки, GUI settle),
/// и за это время сервер может открыть другое окно: слепой клик попал бы в чужое меню.
pub fn try_left_click_expect(bot: &Client, slot: usize, expect_id: i32) -> bool {
    let Some(cur) = try_id(bot) else {
        return false;
    };
    if !window_matches(cur, expect_id) {
        return false;
    }
    try_left_click(bot, slot)
}

pub fn try_shift_click(bot: &Client, slot: usize) -> bool {
    let Some(h) = try_handle(bot) else {
        return false;
    };
    if h.id() == 0 {
        return false;
    }
    h.shift_click(slot);
    true
}

#[cfg(test)]
mod window_guard_tests {
    use super::*;

    #[test]
    fn click_only_in_the_expected_window() {
        assert!(window_matches(7, 7));
        assert!(!window_matches(8, 7)); // открылось другое окно
        assert!(!window_matches(0, 0)); // меню игрока — не контейнер
        assert!(!window_matches(0, 7)); // окно закрылось
    }
}
